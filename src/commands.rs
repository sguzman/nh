use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use owo_colors::OwoColorize;
use subprocess::{Exec, ExitStatus, Redirection};
use thiserror::Error;
use tracing::debug;

use crate::installable::Installable;
use crate::interface::NixBuildPassthroughArgs;

fn ssh_wrap(cmd: Exec, ssh: Option<&str>) -> Exec {
    if let Some(ssh) = ssh {
        Exec::cmd("ssh")
            .arg("-T")
            .arg(ssh)
            .stdin(cmd.to_cmdline_lossy().as_str())
    } else {
        cmd
    }
}

#[allow(dead_code)] // shut up
#[derive(Debug, Clone)]
pub enum EnvAction {
    /// Set an environment variable to a specific value
    Set(String),

    /// Preserve an environment variable from the current environment
    Preserve,

    /// Remove/unset an environment variable
    Remove,
}

#[derive(Debug)]
pub struct Command {
    dry: bool,
    message: Option<String>,
    command: OsString,
    args: Vec<OsString>,
    elevate: bool,
    ssh: Option<String>,
    show_output: bool,
    env_vars: HashMap<String, EnvAction>,
}

impl Command {
    pub fn new<S: AsRef<OsStr>>(command: S) -> Self {
        Self {
            dry: false,
            message: None,
            command: command.as_ref().to_os_string(),
            args: vec![],
            elevate: false,
            ssh: None,
            show_output: false,
            env_vars: HashMap::new(),
        }
    }

    #[must_use]
    pub fn elevate(mut self, elevate: bool) -> Self {
        self.elevate = elevate;
        self
    }

    #[must_use]
    pub fn dry(mut self, dry: bool) -> Self {
        self.dry = dry;
        self
    }

    #[must_use]
    pub fn show_output(mut self, show_output: bool) -> Self {
        self.show_output = show_output;
        self
    }

    #[must_use]
    pub fn ssh(mut self, ssh: Option<String>) -> Self {
        self.ssh = ssh;
        self
    }

    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        for elem in args {
            self.args.push(elem.as_ref().to_os_string());
        }
        self
    }

    pub fn message<S: AsRef<str>>(mut self, message: S) -> Self {
        self.message = Some(message.as_ref().to_string());
        self
    }

    /// Preserve multiple environment variables from the current environment
    pub fn preserve_envs<I, K>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: AsRef<str>,
    {
        for key in keys {
            self.env_vars
                .insert(key.as_ref().to_string(), EnvAction::Preserve);
        }
        self
    }

    /// Configure environment for Nix and NH operations
    #[must_use]
    pub fn with_required_env(mut self) -> Self {
        const ENV: &[&str] = &[
            // This is not a part of Nix's environment, but it might be necessary.
            // nixos-rebuild preserves it, so we do too.
            "LOCALE_ARCHIVE",
            // PATH needs to be preserved so that NH can invoke CLI utilities.
            "PATH",
            // Make sure NIX_SSHOPTS applies to nix commands that invoke ssh, such as `nix copy`
            "NIX_SSHOPTS",
            // This is relevant for Home-Manager systems
            "HOME_MANAGER_BACKUP_EXT",
            // Preserve other Nix-related environment variables
            // TODO: is this everything we need? Previously we only preserved *some* variables
            // and nh continued to work, but any missing vars might break functionality completely
            // unexpectedly. This list could change at any moment. This better be enough. Ugh.
            "NIX_CONFIG",
            "NIX_PATH",
            "NIX_REMOTE",
            "NIX_SSL_CERT_FILE",
            "NIX_USER_CONF_FILES",
        ];

        let env_vars = std::env::vars()
            .filter_map(|(key, value)| match key.as_str() {
                "USER" => Some((key, EnvAction::Set(value))),
                k if ENV.contains(&k) => Some((key, EnvAction::Preserve)),
                k if k.starts_with("NH_") => Some((key, EnvAction::Set(value))),
                _ => None,
            })
            .collect::<Vec<_>>();

        // Only propagate HOME for non-elevated commands
        if !self.elevate {
            if let Ok(home) = std::env::var("HOME") {
                self.env_vars
                    .insert("HOME".to_string(), EnvAction::Set(home));
            }
        }

        debug!(
            "Preserved envs: {}",
            env_vars
                .iter()
                .map(|(key, action)| match action {
                    EnvAction::Set(value) => format!("{key}={value}"),
                    EnvAction::Preserve => format!("{key}=<preserved>"),
                    EnvAction::Remove => format!("{key}=<removed>"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );

        for (key, action) in env_vars {
            self.env_vars.insert(key, action);
        }

        self
    }

    fn apply_env_to_exec(&self, mut cmd: Exec) -> Exec {
        for (key, action) in &self.env_vars {
            match action {
                EnvAction::Set(value) => {
                    cmd = cmd.env(key, value);
                }
                EnvAction::Preserve => {
                    if let Ok(value) = std::env::var(key) {
                        cmd = cmd.env(key, value);
                    }
                }
                EnvAction::Remove => {
                    // For remove, we'll handle this in the sudo construction
                    // by not including it in preserved variables
                }
            }
        }
        cmd
    }

    fn build_sudo_cmd(&self) -> Exec {
        let mut cmd = Exec::cmd("sudo");

        // Collect variables to preserve for sudo
        let mut preserve_vars = Vec::new();
        let mut explicit_env_vars = HashMap::new();

        for (key, action) in &self.env_vars {
            match action {
                EnvAction::Set(value) => {
                    explicit_env_vars.insert(key.clone(), value.clone());
                }
                EnvAction::Preserve => {
                    preserve_vars.push(key.as_str());
                }
                EnvAction::Remove => {
                    // Explicitly don't add to preserve_vars
                }
            }
        }

        if cfg!(target_os = "macos") {
            // Check for if sudo has the preserve-env flag
            let has_preserve_env = Exec::cmd("sudo")
                .args(&["--help"])
                .stderr(Redirection::None)
                .stdout(Redirection::Pipe)
                .capture()
                .map(|output| output.stdout_str().contains("--preserve-env"))
                .unwrap_or(false);

            if has_preserve_env && !preserve_vars.is_empty() {
                cmd = cmd.args(&[
                    "--set-home",
                    &format!("--preserve-env={}", preserve_vars.join(",")),
                ]);
            } else {
                cmd = cmd.arg("--set-home");
            }
        } else {
            // On Linux, use specific environment preservation
            if !preserve_vars.is_empty() {
                cmd = cmd.arg(format!("--preserve-env={}", preserve_vars.join(",")));
            }
        }

        // Use NH_SUDO_ASKPASS program for sudo if present
        if let Ok(askpass) = std::env::var("NH_SUDO_ASKPASS") {
            cmd = cmd.env("SUDO_ASKPASS", askpass).arg("-A");
        }

        // Insert 'env' command to explicitly pass environment variables to the elevated command
        if !explicit_env_vars.is_empty() {
            cmd = cmd.arg("env");
            for (key, value) in explicit_env_vars {
                cmd = cmd.arg(format!("{key}={value}"));
            }
        }

        cmd
    }

    /// Create a sudo command for self-elevation with proper environment handling
    #[must_use]
    pub fn self_elevate_cmd() -> std::process::Command {
        // Get the current executable path
        let current_exe = std::env::current_exe().expect("Failed to get current executable path");

        // Self-elevation with proper environment handling
        let cmd_builder = Self::new(&current_exe).elevate(true).with_required_env();

        let sudo_exec = cmd_builder.build_sudo_cmd();

        // Add the target executable and arguments to the sudo command
        let exec_with_args = sudo_exec.arg(&current_exe);
        let args: Vec<String> = std::env::args().skip(1).collect();
        let final_exec = exec_with_args.args(&args);

        // Convert Exec to std::process::Command by parsing the command line
        let cmdline = final_exec.to_cmdline_lossy();
        let parts: Vec<&str> = cmdline.split_whitespace().collect();

        assert!(!parts.is_empty(), "Failed to build sudo command");

        let mut std_cmd = std::process::Command::new(parts[0]);
        if parts.len() > 1 {
            std_cmd.args(&parts[1..]);
        }

        std_cmd
    }

    pub fn run(&self) -> Result<()> {
        let cmd = if self.elevate {
            self.build_sudo_cmd().arg(&self.command).args(&self.args)
        } else {
            self.apply_env_to_exec(Exec::cmd(&self.command).args(&self.args))
        };

        // Configure output redirection based on show_output setting
        let cmd = ssh_wrap(
            if self.show_output {
                cmd.stderr(Redirection::Merge)
            } else {
                cmd.stderr(Redirection::None).stdout(Redirection::None)
            },
            self.ssh.as_deref(),
        );

        if let Some(m) = &self.message {
            println!("{} {m}", ">".green());
        }

        debug!(?cmd);

        if self.dry {
            return Ok(());
        }

        let msg = self
            .message
            .clone()
            .unwrap_or_else(|| "Command failed".to_string());
        let res = cmd.capture();
        if let Err(e) = res {
            return Err(e).wrap_err(msg);
        }

        let status = &res.as_ref().unwrap().exit_status;
        if !status.success() {
            let stderr = res
                .as_ref()
                .map(subprocess::CaptureData::stderr_str)
                .unwrap_or_default();
            if stderr.trim().is_empty() {
                bail!("{} (exit status {:?})", msg, status);
            }
            bail!("{} (exit status {:?})\nstderr:\n{}", msg, status, stderr);
        }

        Ok(())
    }

    pub fn run_capture(&self) -> Result<Option<String>> {
        let cmd = self.apply_env_to_exec(
            Exec::cmd(&self.command)
                .args(&self.args)
                .stderr(Redirection::None)
                .stdout(Redirection::Pipe),
        );

        if let Some(m) = &self.message {
            println!("{} {m}", ">".green());
        }

        debug!(?cmd);

        if self.dry {
            return Ok(None);
        }
        Ok(Some(cmd.capture()?.stdout_str()))
    }
}

#[derive(Debug)]
pub struct Build {
    message: Option<String>,
    installable: Installable,
    extra_args: Vec<OsString>,
    nom: bool,
    builder: Option<String>,
}

impl Build {
    #[must_use]
    pub const fn new(installable: Installable) -> Self {
        Self {
            message: None,
            installable,
            extra_args: vec![],
            nom: false,
            builder: None,
        }
    }

    pub fn message<S: AsRef<str>>(mut self, message: S) -> Self {
        self.message = Some(message.as_ref().to_string());
        self
    }

    pub fn extra_arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.extra_args.push(arg.as_ref().to_os_string());
        self
    }

    #[must_use]
    pub const fn nom(mut self, yes: bool) -> Self {
        self.nom = yes;
        self
    }

    #[must_use]
    pub fn builder(mut self, builder: Option<String>) -> Self {
        self.builder = builder;
        self
    }

    pub fn extra_args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        for elem in args {
            self.extra_args.push(elem.as_ref().to_os_string());
        }
        self
    }

    #[must_use]
    pub fn passthrough(self, passthrough: &NixBuildPassthroughArgs) -> Self {
        self.extra_args(passthrough.generate_passthrough_args())
    }

    pub fn run(&self) -> Result<()> {
        if let Some(m) = &self.message {
            println!("{} {m}", ">".green());
        }

        let installable_args = self.installable.to_args();

        let base_command = Exec::cmd("nix")
            .arg("build")
            .args(&installable_args)
            .args(&match &self.builder {
                Some(host) => {
                    vec!["--builders".to_string(), format!("ssh://{host} - - - 100")]
                }
                None => vec![],
            })
            .args(&self.extra_args);

        let exit = if self.nom {
            let cmd = {
                base_command
                    .args(&["--log-format", "internal-json", "--verbose"])
                    .stderr(Redirection::Merge)
                    .stdout(Redirection::Pipe)
                    | Exec::cmd("nom").args(&["--json"])
            }
            .stdout(Redirection::None);
            debug!(?cmd);
            cmd.join()
        } else {
            let cmd = base_command
                .stderr(Redirection::Merge)
                .stdout(Redirection::None);

            debug!(?cmd);
            cmd.join()
        };

        match exit? {
            ExitStatus::Exited(0) => (),
            other => bail!(ExitError(other)),
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("Command exited with status {0:?}")]
pub struct ExitError(ExitStatus);

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::OsString;

    use serial_test::serial;

    use super::*;

    // Safely manage environment variables in tests
    struct EnvGuard {
        key: String,
        original: Option<String>,
    }

    impl EnvGuard {
        fn new(key: &str, value: &str) -> Self {
            let original = env::var(key).ok();
            unsafe {
                env::set_var(key, value);
            }
            EnvGuard {
                key: key.to_string(),
                original,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(val) => env::set_var(&self.key, val),
                    None => env::remove_var(&self.key),
                }
            }
        }
    }

    #[test]
    fn test_env_action_variants() {
        // Test that all EnvAction variants are correctly created
        let set_action = EnvAction::Set("test_value".to_string());
        let preserve_action = EnvAction::Preserve;
        let remove_action = EnvAction::Remove;

        match set_action {
            EnvAction::Set(val) => assert_eq!(val, "test_value"),
            _ => panic!("Expected Set variant"),
        }

        assert!(matches!(preserve_action, EnvAction::Preserve));
        assert!(matches!(remove_action, EnvAction::Remove));
    }

    #[test]
    fn test_command_new() {
        let cmd = Command::new("test-command");

        assert_eq!(cmd.command, OsString::from("test-command"));
        assert!(!cmd.dry);
        assert!(cmd.message.is_none());
        assert!(cmd.args.is_empty());
        assert!(!cmd.elevate);
        assert!(cmd.ssh.is_none());
        assert!(!cmd.show_output);
        assert!(cmd.env_vars.is_empty());
    }

    #[test]
    fn test_command_builder_pattern() {
        let cmd = Command::new("test")
            .dry(true)
            .elevate(true)
            .show_output(true)
            .ssh(Some("host".to_string()))
            .message("test message")
            .arg("arg1")
            .args(["arg2", "arg3"]);

        assert!(cmd.dry);
        assert!(cmd.elevate);
        assert!(cmd.show_output);
        assert_eq!(cmd.ssh, Some("host".to_string()));
        assert_eq!(cmd.message, Some("test message".to_string()));
        assert_eq!(
            cmd.args,
            vec![
                OsString::from("arg1"),
                OsString::from("arg2"),
                OsString::from("arg3")
            ]
        );
    }

    #[test]
    fn test_preserve_envs() {
        let cmd = Command::new("test").preserve_envs(["VAR1", "VAR2", "VAR3"]);

        assert_eq!(cmd.env_vars.len(), 3);
        assert!(matches!(
            cmd.env_vars.get("VAR1"),
            Some(EnvAction::Preserve)
        ));
        assert!(matches!(
            cmd.env_vars.get("VAR2"),
            Some(EnvAction::Preserve)
        ));
        assert!(matches!(
            cmd.env_vars.get("VAR3"),
            Some(EnvAction::Preserve)
        ));
    }

    #[test]
    #[serial]
    fn test_with_required_env_home_user() {
        let _home_guard = EnvGuard::new("HOME", "/test/home");
        let _user_guard = EnvGuard::new("USER", "testuser");

        let cmd = Command::new("test").with_required_env();

        // Should preserve HOME and USER as Set actions
        assert!(
            matches!(cmd.env_vars.get("HOME"), Some(EnvAction::Set(val)) if val == "/test/home")
        );
        assert!(matches!(cmd.env_vars.get("USER"), Some(EnvAction::Set(val)) if val == "testuser"));

        // Should preserve all Nix-related variables if present
        for key in [
            "PATH",
            "NIX_CONFIG",
            "NIX_PATH",
            "NIX_REMOTE",
            "NIX_SSHOPTS",
            "NIX_SSL_CERT_FILE",
            "NIX_USER_CONF_FILES",
            "LOCALE_ARCHIVE",
            "HOME_MANAGER_BACKUP_EXT",
        ] {
            if cmd.env_vars.contains_key(key) {
                assert!(matches!(cmd.env_vars.get(key), Some(EnvAction::Preserve)));
            }
        }
    }

    #[test]
    #[serial]
    fn test_with_required_env_missing_home_user() {
        // Test behavior when HOME/USER are not set
        unsafe {
            env::remove_var("HOME");
            env::remove_var("USER");
        }

        let cmd = Command::new("test").with_required_env();

        // Should not have HOME or USER in env_vars if they're not set
        assert!(!cmd.env_vars.contains_key("HOME"));
        assert!(!cmd.env_vars.contains_key("USER"));

        // Should preserve Nix-related variables if present
        for key in [
            "PATH",
            "NIX_CONFIG",
            "NIX_PATH",
            "NIX_REMOTE",
            "NIX_SSHOPTS",
            "NIX_SSL_CERT_FILE",
            "NIX_USER_CONF_FILES",
            "LOCALE_ARCHIVE",
            "HOME_MANAGER_BACKUP_EXT",
        ] {
            if let Some(action) = cmd.env_vars.get(key) {
                assert!(matches!(action, EnvAction::Preserve));
            }
        }
    }

    #[test]
    #[serial]
    fn test_with_required_env_nh_vars() {
        let _guard1 = EnvGuard::new("NH_TEST_VAR", "test_value");
        let _guard2 = EnvGuard::new("NH_ANOTHER_VAR", "another_value");
        let _guard3 = EnvGuard::new("NOT_NH_VAR", "should_not_be_included");

        let cmd = Command::new("test").with_required_env();

        // Should include NH_* variables as Set actions
        assert!(
            matches!(cmd.env_vars.get("NH_TEST_VAR"), Some(EnvAction::Set(val)) if val == "test_value")
        );
        assert!(
            matches!(cmd.env_vars.get("NH_ANOTHER_VAR"), Some(EnvAction::Set(val)) if val == "another_value")
        );

        // Should not include non-NH variables
        assert!(!cmd.env_vars.contains_key("NOT_NH_VAR"));
    }

    #[test]
    #[serial]
    fn test_combined_env_methods() {
        let _home_guard = EnvGuard::new("HOME", "/test/home");
        let _nh_guard = EnvGuard::new("NH_TEST", "nh_value");

        let cmd = Command::new("test")
            .with_required_env()
            .preserve_envs(["EXTRA_VAR"]);

        // Should have HOME from with_nix_env
        assert!(
            matches!(cmd.env_vars.get("HOME"), Some(EnvAction::Set(val)) if val == "/test/home")
        );

        // Should have NH variables from with_nh_env
        assert!(
            matches!(cmd.env_vars.get("NH_TEST"), Some(EnvAction::Set(val)) if val == "nh_value")
        );

        // Should have Nix variables preserved
        assert!(matches!(
            cmd.env_vars.get("PATH"),
            Some(EnvAction::Preserve)
        ));

        // Should have extra preserved variable
        assert!(matches!(
            cmd.env_vars.get("EXTRA_VAR"),
            Some(EnvAction::Preserve)
        ));
    }

    #[test]
    fn test_env_vars_override_behavior() {
        let mut cmd = Command::new("test");

        // First add a variable as Preserve
        cmd.env_vars
            .insert("TEST_VAR".to_string(), EnvAction::Preserve);
        assert!(matches!(
            cmd.env_vars.get("TEST_VAR"),
            Some(EnvAction::Preserve)
        ));

        // Then override it as Set
        cmd.env_vars.insert(
            "TEST_VAR".to_string(),
            EnvAction::Set("new_value".to_string()),
        );
        assert!(
            matches!(cmd.env_vars.get("TEST_VAR"), Some(EnvAction::Set(val)) if val == "new_value")
        );
    }

    #[test]
    fn test_build_sudo_cmd_basic() {
        let cmd = Command::new("test");
        let sudo_exec = cmd.build_sudo_cmd();

        // Platform-agnostic: 'sudo' may not be the first token if env vars are injected (e.g., NH_SUDO_ASKPASS).
        // Accept any command line where 'sudo' is present as a token.
        let cmdline = sudo_exec.to_cmdline_lossy();
        assert!(cmdline.split_whitespace().any(|tok| tok == "sudo"));
    }

    #[test]
    #[serial]
    fn test_build_sudo_cmd_with_preserve_vars() {
        let cmd = Command::new("test").preserve_envs(["VAR1", "VAR2"]);

        let sudo_exec = cmd.build_sudo_cmd();
        let cmdline = sudo_exec.to_cmdline_lossy();

        #[cfg(target_os = "macos")]
        {
            let has_preserve_env = subprocess::Exec::cmd("sudo")
                .args(&["--help"])
                .stderr(subprocess::Redirection::None)
                .stdout(subprocess::Redirection::Pipe)
                .capture()
                .map(|output| output.stdout_str().contains("--preserve-env"))
                .unwrap_or(false);

            if has_preserve_env {
                assert!(cmdline.contains("--preserve-env="));
                assert!(cmdline.contains("VAR1"));
                assert!(cmdline.contains("VAR2"));
            } else {
                assert!(!cmdline.contains("--preserve-env="));
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(cmdline.contains("--preserve-env="));
            assert!(cmdline.contains("VAR1"));
            assert!(cmdline.contains("VAR2"));
        }
    }

    #[test]
    #[serial]
    fn test_build_sudo_cmd_with_set_vars() {
        let mut cmd = Command::new("test");
        cmd.env_vars.insert(
            "TEST_VAR".to_string(),
            EnvAction::Set("test_value".to_string()),
        );

        let sudo_exec = cmd.build_sudo_cmd();
        let cmdline = sudo_exec.to_cmdline_lossy();

        // Should contain env command with variable
        assert!(cmdline.contains("env"));
        assert!(cmdline.contains("TEST_VAR=test_value"));
    }

    #[test]
    #[serial]
    fn test_build_sudo_cmd_with_remove_vars() {
        let mut cmd = Command::new("test");
        cmd.env_vars
            .insert("VAR_TO_PRESERVE".to_string(), EnvAction::Preserve);
        cmd.env_vars
            .insert("VAR_TO_REMOVE".to_string(), EnvAction::Remove);

        let sudo_exec = cmd.build_sudo_cmd();
        let cmdline = sudo_exec.to_cmdline_lossy();

        // Should preserve only the Preserve variable, not the Remove one
        if cmdline.contains("--preserve-env=") {
            assert!(cmdline.contains("VAR_TO_PRESERVE"));
            assert!(!cmdline.contains("VAR_TO_REMOVE"));
        }
    }

    #[test]
    #[serial]
    fn test_build_sudo_cmd_with_askpass() {
        let _guard = EnvGuard::new("NH_SUDO_ASKPASS", "/path/to/askpass");

        let cmd = Command::new("test");
        let sudo_exec = cmd.build_sudo_cmd();
        let cmdline = sudo_exec.to_cmdline_lossy();

        // Should contain -A flag for askpass
        assert!(cmdline.contains("-A"));
    }

    #[test]
    #[serial]
    fn test_build_sudo_cmd_env_added_once() {
        let mut cmd = Command::new("test");
        cmd.env_vars.insert(
            "TEST_VAR1".to_string(),
            EnvAction::Set("value1".to_string()),
        );
        cmd.env_vars.insert(
            "TEST_VAR2".to_string(),
            EnvAction::Set("value2".to_string()),
        );
        cmd.env_vars
            .insert("PRESERVE_VAR".to_string(), EnvAction::Preserve);

        let sudo_exec = cmd.build_sudo_cmd();
        let cmdline = sudo_exec.to_cmdline_lossy();

        // Count occurrences of "env" in the command line
        let env_count = cmdline.matches(" env ").count()
            + usize::from(cmdline.starts_with("env "))
            + usize::from(cmdline.ends_with(" env"));

        // Should contain env command exactly once when there are explicit environment variables
        assert_eq!(
            env_count, 1,
            "env command should appear exactly once in: {cmdline}"
        );

        // Should contain our explicit environment variables
        assert!(cmdline.contains("TEST_VAR1=value1"));
        assert!(cmdline.contains("TEST_VAR2=value2"));
    }

    #[test]
    fn test_build_new() {
        let installable = Installable::Flake {
            reference: "github:user/repo".to_string(),
            attribute: vec!["package".to_string()],
        };

        let build = Build::new(installable.clone());

        assert!(build.message.is_none());
        assert_eq!(build.installable.to_args(), installable.to_args());
        assert!(build.extra_args.is_empty());
        assert!(!build.nom);
        assert!(build.builder.is_none());
    }

    #[test]
    fn test_build_builder_pattern() {
        let installable = Installable::Flake {
            reference: "github:user/repo".to_string(),
            attribute: vec!["package".to_string()],
        };

        let build = Build::new(installable)
            .message("Building package")
            .extra_arg("--verbose")
            .extra_args(["--option", "setting", "value"])
            .nom(true)
            .builder(Some("build-host".to_string()));

        assert_eq!(build.message, Some("Building package".to_string()));
        assert_eq!(
            build.extra_args,
            vec![
                OsString::from("--verbose"),
                OsString::from("--option"),
                OsString::from("setting"),
                OsString::from("value")
            ]
        );
        assert!(build.nom);
        assert_eq!(build.builder, Some("build-host".to_string()));
    }

    #[test]
    fn test_ssh_wrap_with_ssh() {
        let cmd = subprocess::Exec::cmd("echo").arg("hello");
        let wrapped = ssh_wrap(cmd, Some("user@host"));

        let cmdline = wrapped.to_cmdline_lossy();
        assert!(cmdline.starts_with("ssh"));
        assert!(cmdline.contains("-T"));
        assert!(cmdline.contains("user@host"));
    }

    #[test]
    fn test_ssh_wrap_without_ssh() {
        let cmd = subprocess::Exec::cmd("echo").arg("hello");
        let wrapped = ssh_wrap(cmd.clone(), None);

        // Should return the original command unchanged
        assert_eq!(wrapped.to_cmdline_lossy(), cmd.to_cmdline_lossy());
    }

    #[test]
    #[serial]
    fn test_apply_env_to_exec() {
        let _guard = EnvGuard::new("EXISTING_VAR", "existing_value");

        let mut cmd = Command::new("test");
        cmd.env_vars.insert(
            "SET_VAR".to_string(),
            EnvAction::Set("set_value".to_string()),
        );
        cmd.env_vars
            .insert("EXISTING_VAR".to_string(), EnvAction::Preserve);
        cmd.env_vars
            .insert("MISSING_VAR".to_string(), EnvAction::Preserve);
        cmd.env_vars
            .insert("REMOVE_VAR".to_string(), EnvAction::Remove);

        let exec = subprocess::Exec::cmd("echo");
        let result = cmd.apply_env_to_exec(exec);

        // We *can't* easily test the exact environment variables set on Exec,
        // but we *can* verify the method doesn't panic and returns an Exec
        let cmdline = result.to_cmdline_lossy();
        assert!(
            cmdline.contains("echo"),
            "Command line should contain 'echo': {cmdline}"
        );
    }

    #[test]
    fn test_exit_error_display() {
        let exit_status = subprocess::ExitStatus::Exited(1);
        let error = ExitError(exit_status);

        let error_string = format!("{error}");
        assert!(error_string.contains("Command exited with status"));
        assert!(error_string.contains("Exited(1)"));
    }

    #[test]
    fn test_env_action_debug() {
        let set_action = EnvAction::Set("value".to_string());
        let preserve_action = EnvAction::Preserve;
        let remove_action = EnvAction::Remove;

        // Test that Debug is implemented (this will compile-fail if not)
        let _debug_set = format!("{set_action:?}");
        let _debug_preserve = format!("{preserve_action:?}");
        let _debug_remove = format!("{remove_action:?}");
    }

    #[test]
    fn test_env_action_clone() {
        let original = EnvAction::Set("value".to_string());
        let cloned = original.clone();

        match (original, cloned) {
            (EnvAction::Set(orig_val), EnvAction::Set(cloned_val)) => {
                assert_eq!(orig_val, cloned_val);
            }
            _ => panic!("Clone should preserve variant and value"),
        }
    }
}
