use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;

use color_eyre::{eyre, Result};
use tempfile::TempDir;

/// Retrieves the installed Nix version as a string.
///
/// This function executes the `nix --version` command, parses the output to extract the version string,
/// and returns it. If the version string cannot be found or parsed, it returns an error.
///
/// # Returns
///
/// * `Result<String>` - The Nix version string or an error if the version cannot be retrieved.
pub fn get_nix_version() -> Result<String> {
    let output = Command::new("nix").arg("--version").output()?;

    let output_str = str::from_utf8(&output.stdout)?;
    let version_str = output_str
        .lines()
        .next()
        .ok_or_else(|| eyre::eyre!("No version string found"))?;

    // Extract the version substring using a regular expression
    let re = regex::Regex::new(r"\d+\.\d+\.\d+")?;
    if let Some(captures) = re.captures(version_str) {
        let version = captures
            .get(0)
            .ok_or_else(|| eyre::eyre!("No version match found"))?
            .as_str();
        return Ok(version.to_string());
    }

    Err(eyre::eyre!("Failed to extract version"))
}

/// Determines if the Nix binary is actually Lix
///
/// # Returns
///
/// * `Result<bool>` - True if the binary is Lix, false if it's standard Nix
pub fn is_lix() -> Result<bool> {
    let output = Command::new("nix").arg("--version").output()?;
    let output_str = str::from_utf8(&output.stdout)?.to_lowercase();

    Ok(output_str.contains("lix"))
}

/// Represents an object that may be a temporary path
pub trait MaybeTempPath: std::fmt::Debug {
    fn get_path(&self) -> &Path;
}

impl MaybeTempPath for PathBuf {
    fn get_path(&self) -> &Path {
        self.as_ref()
    }
}

impl MaybeTempPath for (PathBuf, TempDir) {
    fn get_path(&self) -> &Path {
        self.0.as_ref()
    }
}

/// Gets the hostname of the current system
///
/// # Returns
///
/// * `Result<String>` - The hostname as a string or an error
pub fn get_hostname() -> Result<String> {
    #[cfg(not(target_os = "macos"))]
    {
        use color_eyre::eyre::Context;
        Ok(hostname::get()
            .context("Failed to get hostname")?
            .to_str()
            .unwrap()
            .to_string())
    }
    #[cfg(target_os = "macos")]
    {
        use color_eyre::eyre::bail;
        use system_configuration::{
            core_foundation::{base::TCFType, string::CFString},
            sys::dynamic_store_copy_specific::SCDynamicStoreCopyLocalHostName,
        };

        let ptr = unsafe { SCDynamicStoreCopyLocalHostName(std::ptr::null()) };
        if ptr.is_null() {
            bail!("Failed to get hostname");
        }
        let name = unsafe { CFString::wrap_under_get_rule(ptr) };

        Ok(name.to_string())
    }
}

/// Retrieves all enabled experimental features in Nix.
///
/// This function executes the `nix config show experimental-features` command and returns
/// a HashSet of the enabled features.
///
/// # Returns
///
/// * `Result<HashSet<String>>` - A HashSet of enabled experimental features or an error.
pub fn get_nix_experimental_features() -> Result<HashSet<String>> {
    let output = Command::new("nix")
        .args(["config", "show", "experimental-features"])
        .output()?;

    if !output.status.success() {
        return Err(eyre::eyre!(
            "Failed to get experimental features: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let output_str = str::from_utf8(&output.stdout)?;
    let enabled_features: HashSet<String> =
        output_str.split_whitespace().map(String::from).collect();

    Ok(enabled_features)
}

/// Checks if all specified experimental features are enabled in Nix.
///
/// # Arguments
///
/// * `features` - A slice of string slices representing the features to check for.
///
/// # Returns
///
/// * `Result<bool>` - True if all specified features are enabled, false otherwise.
pub fn has_all_experimental_features(features: &[&str]) -> Result<bool> {
    let enabled_features = get_nix_experimental_features()?;
    let features_set: HashSet<String> = features.iter().map(|&s| s.to_string()).collect();

    // Check if features_set is a subset of enabled_features
    Ok(features_set.is_subset(&enabled_features))
}
