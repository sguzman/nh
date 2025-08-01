use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, bail};
use color_eyre::eyre::{Result, eyre};
use tracing::{debug, info, warn};

use crate::commands;
use crate::commands::Command;
use crate::generations;
use crate::installable::Installable;
use crate::interface::OsSubcommand::{self};
use crate::interface::{
    self, DiffType, OsBuildVmArgs, OsGenerationsArgs, OsRebuildArgs, OsReplArgs, OsRollbackArgs,
};
use crate::update::update;
use crate::util::ensure_ssh_key_login;
use crate::util::{get_hostname, print_dix_diff};

const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
const CURRENT_PROFILE: &str = "/run/current-system";

const SPEC_LOCATION: &str = "/etc/specialisation";

impl interface::OsArgs {
    pub fn run(self) -> Result<()> {
        use OsRebuildVariant::{Boot, Build, Switch, Test};
        match self.subcommand {
            OsSubcommand::Boot(args) => args.rebuild(&Boot, None),
            OsSubcommand::Test(args) => args.rebuild(&Test, None),
            OsSubcommand::Switch(args) => args.rebuild(&Switch, None),
            OsSubcommand::Build(args) => {
                if args.common.ask || args.common.dry {
                    warn!("`--ask` and `--dry` have no effect for `nh os build`");
                }
                args.rebuild(&Build, None)
            }
            OsSubcommand::BuildVm(args) => args.build_vm(),
            OsSubcommand::Repl(args) => args.run(),
            OsSubcommand::Info(args) => args.info(),
            OsSubcommand::Rollback(args) => args.rollback(),
        }
    }
}

#[derive(Debug)]
enum OsRebuildVariant {
    Build,
    Switch,
    Boot,
    Test,
    BuildVm,
}

impl OsBuildVmArgs {
    fn build_vm(self) -> Result<()> {
        let final_attr = get_final_attr(true, self.with_bootloader);
        debug!("Building VM with attribute: {}", final_attr);
        self.common
            .rebuild(&OsRebuildVariant::BuildVm, Some(final_attr))
    }
}

impl OsRebuildArgs {
    // final_attr is the attribute of config.system.build.X to evaluate.
    fn rebuild(self, variant: &OsRebuildVariant, final_attr: Option<String>) -> Result<()> {
        use OsRebuildVariant::{Boot, Build, BuildVm, Switch, Test};

        if self.build_host.is_some() || self.target_host.is_some() {
            // if it fails its okay
            let _ = ensure_ssh_key_login();
        }

        let elevate = if self.bypass_root_check {
            warn!("Bypassing root check, now running nix as root");
            false
        } else {
            if nix::unistd::Uid::effective().is_root() {
                bail!("Don't run nh os as root. I will call sudo internally as needed");
            }
            true
        };

        if self.update_args.update_all || self.update_args.update_input.is_some() {
            update(&self.common.installable, self.update_args.update_input)?;
        }

        let system_hostname = match get_hostname() {
            Ok(hostname) => Some(hostname),
            Err(err) => {
                tracing::warn!("{}", err.to_string());
                None
            }
        };

        let target_hostname = match &self.hostname {
            Some(h) => h.to_owned(),
            None => match &system_hostname {
                Some(hostname) => {
                    // Only show the warning if we're explicitly building a VM
                    // by directly calling build_vm(), not when the BuildVm variant
                    // is used internally via other code paths
                    if matches!(variant, OsRebuildVariant::BuildVm)
                        && final_attr
                            .as_deref()
                            .is_some_and(|attr| attr == "vm" || attr == "vmWithBootLoader")
                    {
                        tracing::warn!(
                            "Guessing system is {hostname} for a VM image. If this isn't intended, use --hostname to change."
                        );
                    }
                    hostname.clone()
                }
                None => return Err(eyre!("Unable to fetch hostname, and no hostname supplied.")),
            },
        };

        let out_path: Box<dyn crate::util::MaybeTempPath> = match self.common.out_link {
            Some(ref p) => Box::new(p.clone()),
            None => match variant {
                BuildVm | Build => Box::new(PathBuf::from("result")),
                _ => Box::new({
                    let dir = tempfile::Builder::new().prefix("nh-os").tempdir()?;
                    (dir.as_ref().join("result"), dir)
                }),
            },
        };

        debug!(?out_path);

        // Use NH_OS_FLAKE if available, otherwise use the provided installable
        let installable = if let Ok(os_flake) = env::var("NH_OS_FLAKE") {
            debug!("Using NH_OS_FLAKE: {}", os_flake);

            let mut elems = os_flake.splitn(2, '#');
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
            &target_hostname,
            installable,
            final_attr.unwrap_or(String::from("toplevel")).as_str(),
        );

        let message = match variant {
            BuildVm => "Building NixOS VM image",
            _ => "Building NixOS configuration",
        };

        commands::Build::new(toplevel)
            .extra_arg("--out-link")
            .extra_arg(out_path.get_path())
            .extra_args(&self.extra_args)
            .passthrough(&self.common.passthrough)
            .builder(self.build_host.clone())
            .message(message)
            .nom(!self.common.no_nom)
            .run()
            .wrap_err("Failed to build configuration")?;

        let current_specialisation = std::fs::read_to_string(SPEC_LOCATION).ok();

        let target_specialisation = if self.no_specialisation {
            None
        } else {
            current_specialisation.or_else(|| self.specialisation.clone())
        };

        debug!("target_specialisation: {target_specialisation:?}");

        let target_profile = match &target_specialisation {
            None => out_path.get_path().to_owned(),
            Some(spec) => out_path.get_path().join("specialisation").join(spec),
        };

        debug!("Output path: {}", out_path.get_path().display());
        debug!("Target profile path: {}", target_profile.display());
        debug!("Target profile exists: {}", target_profile.exists());

        // Note: out_path itself is kept alive until the end of this function,
        // which prevents the tempdir (if any) from being dropped early

        if !target_profile
            .try_exists()
            .context("Failed to check if target profile exists")?
        {
            return Err(eyre!(
                "Target profile path does not exist: {}",
                target_profile.display()
            ));
        }

        if system_hostname.is_none_or(|h| h == target_hostname) {
            debug!(
                "Comparing with target profile: {}",
                target_profile.display()
            );

            // Compare changes between current and target generation
            match self.common.diff {
                DiffType::Never => {}
                DiffType::Auto => {
                    if self.target_host.is_none() && self.build_host.is_none() {
                        let _ = print_dix_diff(&PathBuf::from(CURRENT_PROFILE), &target_profile);
                    }
                }
                DiffType::Always => {
                    let _ = print_dix_diff(&PathBuf::from(CURRENT_PROFILE), &target_profile);
                }
            }
        } else {
            debug!("Not running dix as the target hostname is different from the system hostname.");
        }

        if self.common.dry || matches!(variant, Build | BuildVm) {
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

        if let Some(target_host) = &self.target_host {
            Command::new("nix")
                .args([
                    "copy",
                    "--to",
                    format!("ssh://{target_host}").as_str(),
                    target_profile.to_str().unwrap(),
                ])
                .message("Copying configuration to target")
                .with_required_env()
                .run()?;
        }

        if let Test | Switch = variant {
            let switch_to_configuration =
                target_profile.join("bin").join("switch-to-configuration");

            if !switch_to_configuration.exists() {
                return Err(eyre!(
                    "The 'switch-to-configuration binary' is missing from the built configuration.\n

                    This typically happens when system.switch.enable is set to false in your
                    NixOS configuration. To fix this, please either:\n\
                    1. Remove 'system.switch.enable = false' from your configuration, or\n\
                    2. Set 'system.switch.enable = true' explicitly\n

                    If the problem persists, please open an issue on our issue tracker!
                    "
                ));
            }

            let switch_to_configuration = switch_to_configuration
                .canonicalize()
                .context("Failed to resolve switch-to-configuration path")?;
            let switch_to_configuration = switch_to_configuration
                .to_str()
                .ok_or_else(|| eyre!("switch-to-configuration path contains invalid UTF-8"))?;

            Command::new(switch_to_configuration)
                .arg("test")
                .ssh(self.target_host.clone())
                .message("Activating configuration")
                .elevate(elevate)
                .preserve_envs(["NIXOS_INSTALL_BOOTLOADER"])
                .with_required_env()
                .run()
                .wrap_err("Activation (test) failed")?;
        }

        if let Boot | Switch = variant {
            let canonical_out_path = out_path
                .get_path()
                .canonicalize()
                .context("Failed to resolve output path")?;

            Command::new("nix")
                .elevate(elevate)
                .args(["build", "--no-link", "--profile", SYSTEM_PROFILE])
                .arg(&canonical_out_path)
                .ssh(self.target_host.clone())
                .with_required_env()
                .run()
                .wrap_err("Failed to set system profile")?;

            let switch_to_configuration = out_path
                .get_path()
                .join("bin")
                .join("switch-to-configuration");

            if !switch_to_configuration.exists() {
                return Err(eyre!(
                    "The switch-to-configuration binary is missing from the built configuration.\n\
                    This typically happens when system.switch.enable is set to false in your NixOS configuration.\n\
                    To fix this, either:\n\
                    1. Remove 'system.switch.enable = false;' from your configuration, or\n\
                    2. Set 'system.switch.enable = true;' explicitly"
                ));
            }

            let switch_to_configuration = switch_to_configuration
                .canonicalize()
                .context("Failed to resolve switch-to-configuration path")?;
            let switch_to_configuration = switch_to_configuration
                .to_str()
                .ok_or_else(|| eyre!("switch-to-configuration path contains invalid UTF-8"))?;

            Command::new(switch_to_configuration)
                .arg("boot")
                .ssh(self.target_host)
                .elevate(elevate)
                .message("Adding configuration to bootloader")
                .preserve_envs(["NIXOS_INSTALL_BOOTLOADER"])
                .with_required_env()
                .run()
                .wrap_err("Bootloader activation failed")?;
        }

        // Make sure out_path is not accidentally dropped
        // https://docs.rs/tempfile/3.12.0/tempfile/index.html#early-drop-pitfall
        debug!(
            "Completed operation with output path: {:?}",
            out_path.get_path()
        );
        drop(out_path);

        Ok(())
    }
}

impl OsRollbackArgs {
    fn rollback(&self) -> Result<()> {
        let elevate = if self.bypass_root_check {
            warn!("Bypassing root check, now running nix as root");
            false
        } else {
            if nix::unistd::Uid::effective().is_root() {
                bail!("Don't run nh os as root. I will call sudo internally as needed");
            }
            true
        };

        // Find previous generation or specific generation
        let target_generation = if let Some(gen_number) = self.to {
            find_generation_by_number(gen_number)?
        } else {
            find_previous_generation()?
        };

        info!("Rolling back to generation {}", target_generation.number);

        // Construct path to the generation
        let profile_dir = Path::new(SYSTEM_PROFILE)
            .parent()
            .unwrap_or(Path::new("/nix/var/nix/profiles"));
        let generation_link = profile_dir.join(format!("system-{}-link", target_generation.number));

        // Handle specialisations
        let current_specialisation = fs::read_to_string(SPEC_LOCATION).ok();

        let target_specialisation = if self.no_specialisation {
            None
        } else {
            self.specialisation.clone().or(current_specialisation)
        };

        debug!("target_specialisation: {target_specialisation:?}");

        // Compare changes between current and target generation
        match self.diff {
            DiffType::Never => {}
            _ => {
                let _ = print_dix_diff(&PathBuf::from(CURRENT_PROFILE), &generation_link);
            }
        }

        if self.dry {
            info!(
                "Dry run: would roll back to generation {}",
                target_generation.number
            );
            return Ok(());
        }

        if self.ask {
            info!("Roll back to generation {}?", target_generation.number);
            let confirmation = dialoguer::Confirm::new().default(false).interact()?;

            if !confirmation {
                bail!("User rejected the rollback");
            }
        }

        // Get current generation number for potential rollback
        let current_gen_number = match get_current_generation_number() {
            Ok(num) => num,
            Err(e) => {
                warn!("Failed to get current generation number: {}", e);
                0
            }
        };

        // Set the system profile
        info!("Setting system profile...");

        // Instead of direct symlink operations, use a command with proper elevation
        Command::new("ln")
            .arg("-sfn") // force, symbolic link
            .arg(&generation_link)
            .arg(SYSTEM_PROFILE)
            .elevate(elevate)
            .message("Setting system profile")
            .with_required_env()
            .run()
            .wrap_err("Failed to set system profile during rollback")?;

        // Determine the correct profile to use with specialisations
        let final_profile = match &target_specialisation {
            None => generation_link,
            Some(spec) => {
                let spec_path = generation_link.join("specialisation").join(spec);
                if spec_path.exists() {
                    spec_path
                } else {
                    warn!(
                        "Specialisation '{}' does not exist in generation {}",
                        spec, target_generation.number
                    );
                    warn!("Using base configuration without specialisations");
                    generation_link
                }
            }
        };

        // Activate the configuration
        info!("Activating...");

        let switch_to_configuration = final_profile.join("bin").join("switch-to-configuration");

        if !switch_to_configuration.exists() {
            return Err(eyre!(
                "The switch-to-configuration binary is missing from the built configuration.\n\
                    This typically happens when system.switch.enable is set to false in your NixOS configuration.\n\
                    To fix this, either:\n\
                    1. Remove 'system.switch.enable = false;' from your configuration, or\n\
                    2. Set 'system.switch.enable = true;' explicitly"
            ));
        }

        match Command::new(&switch_to_configuration)
            .arg("switch")
            .elevate(elevate)
            .preserve_envs(["NIXOS_INSTALL_BOOTLOADER"])
            .with_required_env()
            .run()
        {
            Ok(()) => {
                info!(
                    "Successfully rolled back to generation {}",
                    target_generation.number
                );
            }
            Err(e) => {
                // If activation fails, rollback the profile
                if current_gen_number > 0 {
                    let current_gen_link =
                        profile_dir.join(format!("system-{current_gen_number}-link"));

                    Command::new("ln")
                        .arg("-sfn") // Force, symbolic link
                        .arg(&current_gen_link)
                        .arg(SYSTEM_PROFILE)
                        .elevate(elevate)
                        .message("Rolling back system profile")
                        .with_required_env()
                        .run()
                        .wrap_err("NixOS: Failed to restore previous system profile after failed activation")?;
                }

                return Err(eyre!("Activation (switch) failed: {}", e))
                    .context("Failed to activate configuration");
            }
        }

        Ok(())
    }
}

fn find_previous_generation() -> Result<generations::GenerationInfo> {
    let profile_path = PathBuf::from(SYSTEM_PROFILE);

    let mut generations: Vec<generations::GenerationInfo> = fs::read_dir(
        profile_path
            .parent()
            .unwrap_or(Path::new("/nix/var/nix/profiles")),
    )?
    .filter_map(|entry| {
        entry.ok().and_then(|e| {
            let path = e.path();
            if let Some(filename) = path.file_name() {
                if let Some(name) = filename.to_str() {
                    if name.starts_with("system-") && name.ends_with("-link") {
                        return generations::describe(&path);
                    }
                }
            }
            None
        })
    })
    .collect();

    if generations.is_empty() {
        bail!("No generations found");
    }

    generations.sort_by(|a, b| {
        a.number
            .parse::<u64>()
            .unwrap_or(0)
            .cmp(&b.number.parse::<u64>().unwrap_or(0))
    });

    let current_idx = generations
        .iter()
        .position(|g| g.current)
        .ok_or_else(|| eyre!("Current generation not found"))?;

    if current_idx == 0 {
        bail!("No generation older than the current one exists");
    }

    Ok(generations[current_idx - 1].clone())
}

fn find_generation_by_number(number: u64) -> Result<generations::GenerationInfo> {
    let profile_path = PathBuf::from(SYSTEM_PROFILE);

    let generations: Vec<generations::GenerationInfo> = fs::read_dir(
        profile_path
            .parent()
            .unwrap_or(Path::new("/nix/var/nix/profiles")),
    )?
    .filter_map(|entry| {
        entry.ok().and_then(|e| {
            let path = e.path();
            if let Some(filename) = path.file_name() {
                if let Some(name) = filename.to_str() {
                    if name.starts_with("system-") && name.ends_with("-link") {
                        return generations::describe(&path);
                    }
                }
            }
            None
        })
    })
    .filter(|generation| generation.number == number.to_string())
    .collect();

    if generations.is_empty() {
        bail!("Generation {} not found", number);
    }

    Ok(generations[0].clone())
}

fn get_current_generation_number() -> Result<u64> {
    let profile_path = PathBuf::from(SYSTEM_PROFILE);

    let generations: Vec<generations::GenerationInfo> = fs::read_dir(
        profile_path
            .parent()
            .unwrap_or(Path::new("/nix/var/nix/profiles")),
    )?
    .filter_map(|entry| entry.ok().and_then(|e| generations::describe(&e.path())))
    .collect();

    let current_gen = generations
        .iter()
        .find(|g| g.current)
        .ok_or_else(|| eyre!("Current generation not found"))?;

    current_gen
        .number
        .parse::<u64>()
        .wrap_err("Invalid generation number")
}

#[must_use]
pub fn get_final_attr(build_vm: bool, with_bootloader: bool) -> String {
    let attr = if build_vm && with_bootloader {
        "vmWithBootLoader"
    } else if build_vm {
        "vm"
    } else {
        "toplevel"
    };
    String::from(attr)
}

pub fn toplevel_for<S: AsRef<str>>(
    hostname: S,
    installable: Installable,
    final_attr: &str,
) -> Installable {
    let mut res = installable;
    let hostname = hostname.as_ref().to_owned();

    let toplevel = ["config", "system", "build", final_attr]
        .into_iter()
        .map(String::from);

    match res {
        Installable::Flake {
            ref mut attribute, ..
        } => {
            // If user explicitly selects some other attribute, don't push nixosConfigurations
            if attribute.is_empty() {
                attribute.push(String::from("nixosConfigurations"));
                attribute.push(hostname);
            }
            attribute.extend(toplevel);
        }
        Installable::File {
            ref mut attribute, ..
        } => {
            attribute.extend(toplevel);
        }
        Installable::Expression {
            ref mut attribute, ..
        } => {
            attribute.extend(toplevel);
        }
        Installable::Store { .. } | Installable::System { .. } => {}
    }

    res
}

impl OsReplArgs {
    fn run(self) -> Result<()> {
        // Use NH_OS_FLAKE if available, otherwise use the provided installable
        let mut target_installable = if let Ok(os_flake) = env::var("NH_OS_FLAKE") {
            debug!("Using NH_OS_FLAKE: {}", os_flake);

            let mut elems = os_flake.splitn(2, '#');
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

        if matches!(
            target_installable,
            Installable::Store { .. } | Installable::System { .. }
        ) {
            bail!("Nix doesn't support nix store installables.");
        }

        let hostname = self.hostname.ok_or(()).or_else(|()| get_hostname())?;

        if let Installable::Flake {
            ref mut attribute, ..
        } = target_installable
        {
            if attribute.is_empty() {
                attribute.push(String::from("nixosConfigurations"));
                attribute.push(hostname);
            }
        }

        Command::new("nix")
            .arg("repl")
            .args(target_installable.to_args())
            .with_required_env()
            .show_output(true)
            .run()?;

        Ok(())
    }
}

impl OsGenerationsArgs {
    fn info(&self) -> Result<()> {
        let profile = match self.profile {
            Some(ref p) => PathBuf::from(p),
            None => bail!("Profile path is required"),
        };

        if !profile.is_symlink() {
            return Err(eyre!(
                "No profile `{:?}` found",
                profile.file_name().unwrap_or_default()
            ));
        }

        let profile_dir = profile.parent().unwrap_or_else(|| Path::new("."));

        let generations: Vec<_> = fs::read_dir(profile_dir)?
            .filter_map(|entry| {
                entry.ok().and_then(|e| {
                    let path = e.path();
                    if path
                        .file_name()?
                        .to_str()?
                        .starts_with(profile.file_name()?.to_str()?)
                    {
                        Some(path)
                    } else {
                        None
                    }
                })
            })
            .collect();

        let descriptions: Vec<generations::GenerationInfo> = generations
            .iter()
            .filter_map(|gen_dir| generations::describe(gen_dir))
            .collect();

        let _ = generations::print_info(descriptions);

        Ok(())
    }
}
