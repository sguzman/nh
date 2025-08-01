use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};
use tracing::{debug, info, warn};

use crate::commands;
use crate::commands::Command;
use crate::installable::Installable;
use crate::interface::{self, DiffType, HomeRebuildArgs, HomeReplArgs, HomeSubcommand};
use crate::update::update;
use crate::util::{get_hostname, print_dix_diff};

impl interface::HomeArgs {
    pub fn run(self) -> Result<()> {
        use HomeRebuildVariant::{Build, Switch};
        match self.subcommand {
            HomeSubcommand::Switch(args) => args.rebuild(&Switch),
            HomeSubcommand::Build(args) => {
                if args.common.ask || args.common.dry {
                    warn!("`--ask` and `--dry` have no effect for `nh home build`");
                }
                args.rebuild(&Build)
            }
            HomeSubcommand::Repl(args) => args.run(),
        }
    }
}

#[derive(Debug)]
enum HomeRebuildVariant {
    Build,
    Switch,
}

impl HomeRebuildArgs {
    fn rebuild(self, variant: &HomeRebuildVariant) -> Result<()> {
        use HomeRebuildVariant::Build;

        if self.update_args.update_all || self.update_args.update_input.is_some() {
            update(&self.common.installable, self.update_args.update_input)?;
        }

        let out_path: Box<dyn crate::util::MaybeTempPath> = match self.common.out_link {
            Some(ref p) => Box::new(p.clone()),
            None => Box::new({
                let dir = tempfile::Builder::new().prefix("nh-home").tempdir()?;
                (dir.as_ref().join("result"), dir)
            }),
        };

        debug!(?out_path);

        // Use NH_HOME_FLAKE if available, otherwise use the provided installable
        let installable = if let Ok(home_flake) = env::var("NH_HOME_FLAKE") {
            debug!("Using NH_HOME_FLAKE: {}", home_flake);

            let mut elems = home_flake.splitn(2, '#');
            let reference = elems.next().unwrap().to_owned();
            let attribute = elems
                .next()
                .map(crate::installable::parse_attribute)
                .unwrap_or_default();

            Installable::Flake {
                reference,
                attribute,
            }
        } else {
            self.common.installable.clone()
        };

        let toplevel = toplevel_for(
            installable,
            true,
            &self.extra_args,
            self.configuration.clone(),
        )?;

        commands::Build::new(toplevel)
            .extra_arg("--out-link")
            .extra_arg(out_path.get_path())
            .extra_args(&self.extra_args)
            .passthrough(&self.common.passthrough)
            .message("Building Home-Manager configuration")
            .nom(!self.common.no_nom)
            .run()
            .wrap_err("Failed to build Home-Manager configuration")?;

        let prev_generation: Option<PathBuf> = [
            PathBuf::from("/nix/var/nix/profiles/per-user")
                .join(env::var("USER").expect("Couldn't get username"))
                .join("home-manager"),
            PathBuf::from(env::var("HOME").expect("Couldn't get home directory"))
                .join(".local/state/nix/profiles/home-manager"),
        ]
        .into_iter()
        .find(|next| next.exists());

        debug!(?prev_generation);

        let spec_location =
            PathBuf::from(std::env::var("HOME")?).join(".local/share/home-manager/specialisation");

        let current_specialisation = std::fs::read_to_string(spec_location.to_str().unwrap()).ok();

        let target_specialisation = if self.no_specialisation {
            None
        } else {
            current_specialisation.or(self.specialisation)
        };

        debug!("target_specialisation: {target_specialisation:?}");

        let target_profile: Box<dyn crate::util::MaybeTempPath> =
            if let Some(spec) = &target_specialisation {
                Box::new(out_path.get_path().join("specialisation").join(spec))
            } else {
                out_path
            };

        // Take a strong reference to ensure the TempDir isn't dropped
        // This prevents early dropping of the tempdir, which causes dix to fail
        #[allow(unused_variables)]
        let keep_alive = target_profile.get_path().to_owned();

        // just do nothing for None case (fresh installs)
        if let Some(generation) = prev_generation {
            match self.common.diff {
                DiffType::Never => {}
                _ => {
                    let _ = print_dix_diff(&generation, target_profile.get_path());
                }
            }
        }

        if self.common.dry || matches!(variant, Build) {
            if self.common.ask {
                warn!("--ask has no effect as dry run was requested");
            }
            return Ok(());
        }

        if self.common.ask {
            info!("Apply the config?");
            let confirmation = dialoguer::Confirm::new().default(false).interact()?;

            if !confirmation {
                bail!("User rejected the new config");
            }
        }

        if let Some(ext) = &self.backup_extension {
            info!("Using {} as the backup extension", ext);
            unsafe {
                env::set_var("HOME_MANAGER_BACKUP_EXT", ext);
            }
        }

        Command::new(target_profile.get_path().join("activate"))
            .with_required_env()
            .message("Activating configuration")
            .run()
            .wrap_err("Activation failed")?;

        // Make sure out_path is not accidentally dropped
        // https://docs.rs/tempfile/3.12.0/tempfile/index.html#early-drop-pitfall
        debug!(
            "Completed operation with output path: {:?}",
            target_profile.get_path()
        );
        drop(target_profile);

        Ok(())
    }
}

fn toplevel_for<I, S>(
    installable: Installable,
    push_drv: bool,
    extra_args: I,
    configuration_name: Option<String>,
) -> Result<Installable>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut res = installable;
    let extra_args: Vec<OsString> = {
        let mut vec = Vec::new();
        for elem in extra_args {
            vec.push(elem.as_ref().to_owned());
        }
        vec
    };

    let toplevel = ["config", "home", "activationPackage"]
        .into_iter()
        .map(String::from);

    match res {
        Installable::Flake {
            ref reference,
            ref mut attribute,
        } => {
            // If user explicitly selects some other attribute in the installable itself
            // then don't push homeConfigurations
            if !attribute.is_empty() {
                debug!(
                    "Using explicit attribute path from installable: {:?}",
                    attribute
                );
                return Ok(res);
            }

            attribute.push(String::from("homeConfigurations"));

            let flake_reference = reference.clone();
            let mut found_config = false;

            // Check if an explicit configuration name was provided via the flag
            if let Some(config_name) = configuration_name {
                // Verify the provided configuration exists
                let func = format!(r#" x: x ? "{config_name}" "#);
                let check_res = commands::Command::new("nix")
                    .with_required_env()
                    .arg("eval")
                    .args(&extra_args)
                    .arg("--apply")
                    .arg(func)
                    .args(
                        (Installable::Flake {
                            reference: flake_reference.clone(),
                            attribute: attribute.clone(),
                        })
                        .to_args(),
                    )
                    .run_capture()
                    .wrap_err(format!(
                        "Failed running nix eval to check for explicit configuration '{config_name}'"
                    ))?;

                if check_res.map(|s| s.trim().to_owned()).as_deref() == Some("true") {
                    debug!("Using explicit configuration from flag: {}", config_name);
                    attribute.push(config_name);
                    if push_drv {
                        attribute.extend(toplevel.clone());
                    }
                    found_config = true;
                } else {
                    // Explicit config provided but not found
                    let tried_attr_path = {
                        let mut attr_path = attribute.clone();
                        attr_path.push(config_name);
                        Installable::Flake {
                            reference: flake_reference,
                            attribute: attr_path,
                        }
                        .to_args()
                        .join(" ")
                    };
                    bail!(
                        "Explicitly specified home-manager configuration not found: {tried_attr_path}"
                    );
                }
            }

            // If no explicit config was found via flag, try automatic detection
            if !found_config {
                let username = std::env::var("USER").expect("Couldn't get username");
                let hostname = get_hostname()?;
                let mut tried = vec![];

                for attr_name in [format!("{username}@{hostname}"), username] {
                    let func = format!(r#" x: x ? "{attr_name}" "#);
                    let check_res = commands::Command::new("nix")
                        .with_required_env()
                        .arg("eval")
                        .args(&extra_args)
                        .arg("--apply")
                        .arg(func)
                        .args(
                            (Installable::Flake {
                                reference: flake_reference.clone(),
                                attribute: attribute.clone(),
                            })
                            .to_args(),
                        )
                        .run_capture()
                        .wrap_err(format!(
                            "Failed running nix eval to check for automatic configuration '{attr_name}'"
                        ))?;

                    let current_try_attr = {
                        let mut attr_path = attribute.clone();
                        attr_path.push(attr_name.clone());
                        attr_path
                    };
                    tried.push(current_try_attr.clone());

                    if let Some("true") = check_res.map(|s| s.trim().to_owned()).as_deref() {
                        debug!("Using automatically detected configuration: {}", attr_name);
                        attribute.push(attr_name);
                        if push_drv {
                            attribute.extend(toplevel.clone());
                        }
                        found_config = true;
                        break;
                    }
                }

                // If still not found after automatic detection, error out
                if !found_config {
                    let tried_str = tried
                        .into_iter()
                        .map(|a| {
                            Installable::Flake {
                                reference: flake_reference.clone(),
                                attribute: a,
                            }
                            .to_args()
                            .join(" ")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "Couldn't find home-manager configuration automatically, tried: {tried_str}"
                    );
                }
            }
        }
        Installable::File {
            ref mut attribute, ..
        } => {
            if push_drv {
                attribute.extend(toplevel);
            }
        }
        Installable::Expression {
            ref mut attribute, ..
        } => {
            if push_drv {
                attribute.extend(toplevel);
            }
        }
        Installable::Store { .. } | Installable::System { .. } => {}
    }

    Ok(res)
}

impl HomeReplArgs {
    fn run(self) -> Result<()> {
        // Use NH_HOME_FLAKE if available, otherwise use the provided installable
        let installable = if let Ok(home_flake) = env::var("NH_HOME_FLAKE") {
            debug!("Using NH_HOME_FLAKE: {}", home_flake);

            let mut elems = home_flake.splitn(2, '#');
            let reference = elems.next().unwrap().to_owned();
            let attribute = elems
                .next()
                .map(crate::installable::parse_attribute)
                .unwrap_or_default();

            Installable::Flake {
                reference,
                attribute,
            }
        } else {
            self.installable
        };

        let toplevel = toplevel_for(
            installable,
            false,
            &self.extra_args,
            self.configuration.clone(),
        )?;

        Command::new("nix")
            .with_required_env()
            .arg("repl")
            .args(toplevel.to_args())
            .show_output(true)
            .run()?;

        Ok(())
    }
}
