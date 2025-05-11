use color_eyre::eyre::bail;
use color_eyre::Result;
use tracing::{debug, info, warn};

use crate::commands::Command;
use crate::interface::{DarwinArgs, DarwinRebuildArgs, DarwinReplArgs, DarwinSubcommand};
use crate::update::update;
use crate::util::platform;

const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
const CURRENT_PROFILE: &str = "/run/current-system";

impl DarwinArgs {
    pub fn run(self) -> Result<()> {
        use DarwinRebuildVariant::{Build, Switch};
        match self.subcommand {
            DarwinSubcommand::Switch(args) => args.rebuild(Switch),
            DarwinSubcommand::Build(args) => {
                if args.common.ask || args.common.dry {
                    warn!("`--ask` and `--dry` have no effect for `nh darwin build`");
                }
                args.rebuild(Build)
            }
            DarwinSubcommand::Repl(args) => args.run(),
        }
    }
}

enum DarwinRebuildVariant {
    Switch,
    Build,
}

impl DarwinRebuildArgs {
    fn rebuild(self, variant: DarwinRebuildVariant) -> Result<()> {
        use DarwinRebuildVariant::{Build, Switch};

        // Check if we're running as root
        platform::check_not_root(false)?;

        if self.update_args.update {
            update(&self.common.installable, self.update_args.update_input)?;
        }

        // Get the hostname
        let (hostname, _) = platform::get_target_hostname(self.hostname, false)?;

        // Create output path
        let out_path = platform::create_output_path(self.common.out_link, "nh-darwin")?;
        debug!(?out_path);

        // Use NH_DARWIN_FLAKE if available, otherwise use the provided installable
        let installable =
            platform::resolve_env_installable("NH_DARWIN_FLAKE", self.common.installable.clone());

        // Build the configuration
        let _target_profile = platform::handle_rebuild_workflow(
            installable,
            "darwinConfigurations",
            &["toplevel"],
            Some(hostname),
            out_path.as_ref(),
            &self.extra_args,
            None, // No builder
            "Building Darwin configuration",
            self.common.no_nom,
            "",    // No specialisation path for Darwin
            false, // No specialisation
            None,  // No specialisation
            CURRENT_PROFILE,
            false, // Don't skip comparison
        )?;

        if self.common.ask && !self.common.dry && !matches!(variant, Build) {
            info!("Apply the config?");
            let confirmation = dialoguer::Confirm::new().default(false).interact()?;

            if !confirmation {
                bail!("User rejected the new config");
            }
        }

        if matches!(variant, Switch) && !self.common.dry {
            Command::new("nix")
                .args(["build", "--no-link", "--profile", SYSTEM_PROFILE])
                .arg(out_path.get_path())
                .elevate(true)
                .dry(self.common.dry)
                .run()?;

            let darwin_rebuild = out_path.get_path().join("sw/bin/darwin-rebuild");
            let activate_user = out_path.get_path().join("activate-user");

            // Determine if we need to elevate privileges
            let needs_elevation = !activate_user.try_exists().unwrap_or(false)
                || std::fs::read_to_string(&activate_user)
                    .unwrap_or_default()
                    .contains("# nix-darwin: deprecated");

            // Create and run the activation command with or without elevation
            Command::new(darwin_rebuild)
                .arg("activate")
                .message("Activating configuration")
                .elevate(needs_elevation)
                .dry(self.common.dry)
                .run()?;
        }

        // Make sure out_path is not accidentally dropped
        // https://docs.rs/tempfile/3.12.0/tempfile/index.html#early-drop-pitfall
        drop(out_path);

        Ok(())
    }
}

impl DarwinReplArgs {
    fn run(self) -> Result<()> {
        // Use NH_DARWIN_FLAKE if available, otherwise use the provided installable
        let installable = platform::resolve_env_installable("NH_DARWIN_FLAKE", self.installable);

        // Get hostname for the configuration
        let hostname = match self.hostname {
            Some(h) => h,
            None => crate::util::get_hostname()?,
        };

        // Start an interactive Nix REPL with the darwin configuration
        platform::run_repl(
            installable,
            "darwinConfigurations",
            &[], // No extra path needed
            Some(hostname),
            &[], // No extra arguments
        )
    }
}
