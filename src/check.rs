use std::cmp::Ordering;

use color_eyre::{eyre, Result};
use semver::Version;

use crate::util;

/// Verifies if the installed Nix version meets requirements
///
/// # Returns
///
/// * `Result<()>` - Ok if version requirements are met, error otherwise
pub fn check_nix_version() -> Result<()> {
    let version = util::get_nix_version()?;
    let is_lix_binary = util::is_lix()?;

    let min_version = if is_lix_binary { "2.91.0" } else { "2.26.1" };

    let current = Version::parse(&version)?;
    let required = Version::parse(min_version)?;

    match current.cmp(&required) {
        Ordering::Less => {
            let binary_name = if is_lix_binary { "Lix" } else { "Nix" };
            Err(eyre::eyre!(
                "{} version {} is too old. Minimum required version is {}",
                binary_name,
                version,
                min_version
            ))
        }
        _ => Ok(()),
    }
}

/// Verifies if the required experimental features are enabled
///
/// # Returns
///
/// * `Result<()>` - Ok if all required features are enabled, error otherwise
pub fn check_nix_features() -> Result<()> {
    let mut required_features = vec!["nix-command", "flakes"];

    // Lix still uses repl-flake, which is removed in the latest version of Nix.
    if util::is_lix()? {
        required_features.push("repl-flake");
    }

    if !util::has_all_experimental_features(&required_features)? {
        return Err(eyre::eyre!(
            "Missing required experimental features. Please enable: {}",
            required_features.join(", ")
        ));
    }

    Ok(())
}

/// Handles environment variable setup and returns if a warning should be shown
///
/// # Returns
///
/// * `Result<bool>` - True if a warning should be shown about the FLAKE variable, false otherwise
pub fn setup_environment() -> Result<bool> {
    let mut do_warn = false;

    if let Ok(f) = std::env::var("FLAKE") {
        // Set NH_FLAKE if it's not already set
        if std::env::var("NH_FLAKE").is_err() {
            std::env::set_var("NH_FLAKE", f);

            // Only warn if FLAKE is set and we're using it to set NH_FLAKE
            // AND none of the command-specific env vars are set
            if std::env::var("NH_OS_FLAKE").is_err()
                && std::env::var("NH_HOME_FLAKE").is_err()
                && std::env::var("NH_DARWIN_FLAKE").is_err()
            {
                do_warn = true;
            }
        }
    }

    Ok(do_warn)
}

/// Runs all necessary checks for Nix functionality
///
/// # Returns
///
/// * `Result<()>` - Ok if all checks pass, error otherwise
pub fn verify_nix_environment() -> Result<()> {
    check_nix_version()?;
    check_nix_features()?;
    Ok(())
}
