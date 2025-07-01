use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use subprocess::{Exec, ExitStatus, Redirection};
use thiserror::Error;
use tracing::{debug, info};

use crate::installable::Installable;

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

    pub const fn elevate(mut self, elevate: bool) -> Self {
        self.elevate = elevate;
        self
    }

    pub const fn dry(mut self, dry: bool) -> Self {
        self.dry = dry;
        self
    }

    pub const fn show_output(mut self, show_output: bool) -> Self {
        self.show_output = show_output;
        self
    }

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

    /// Configure environment for Nix operations with proper HOME handling
    pub fn with_nix_env(mut self) -> Self {
        // Preserve original user's HOME and USER
        if let Ok(home) = std::env::var("HOME") {
            self.env_vars
                .insert("HOME".to_string(), EnvAction::Set(home));
        }
        if let Ok(user) = std::env::var("USER") {
            self.env_vars
                .insert("USER".to_string(), EnvAction::Set(user));
        }

        // Preserve Nix-related environment variables
        // TODO: is this everything we need? Previously we only preserved *some* variables
        // and nh continued to work, but any missing vars might break functionality completely
        // unexpectedly.
        self.preserve_envs([
            "PATH",
            "NIX_CONFIG",
            "NIX_PATH",
            "NIX_REMOTE",
            "NIX_SSL_CERT_FILE",
            "NIX_USER_CONF_FILES",
        ])
    }

    /// Configure environment for NH operations
    pub fn with_nh_env(mut self) -> Self {
        // Preserve all NH_* environment variables
        for (key, value) in std::env::vars() {
            if key.starts_with("NH_") {
                self.env_vars.insert(key, EnvAction::Set(value));
            }
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
                    "env",
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
                cmd = cmd.arg(format!("{}={}", key, value));
            }
        }

        cmd
    }

    /// Create a sudo command for self-elevation with proper environment handling
    pub fn self_elevate_cmd() -> std::process::Command {
        // Get the current executable path
        let current_exe = std::env::current_exe().expect("Failed to get current executable path");

        // Self-elevation with proper environment handling
        let cmd_builder = Self::new(&current_exe)
            .elevate(true)
            .with_nix_env()
            .with_nh_env();

        let sudo_exec = cmd_builder.build_sudo_cmd();

        // Add the target executable and arguments to the sudo command
        let exec_with_args = sudo_exec.arg(&current_exe);
        let args: Vec<String> = std::env::args().skip(1).collect();
        let final_exec = exec_with_args.args(&args);

        // Convert Exec to std::process::Command by parsing the command line
        let cmdline = final_exec.to_cmdline_lossy();
        let parts: Vec<&str> = cmdline.split_whitespace().collect();

        if parts.is_empty() {
            panic!("Failed to build sudo command");
        }

        let mut std_cmd = std::process::Command::new(parts[0]);
        if parts.len() > 1 {
            std_cmd.args(&parts[1..]);
        }

        std_cmd
    }

    pub fn run(&self) -> Result<()> {
        let cmd = if self.elevate {
            let sudo_cmd = self.build_sudo_cmd();
            sudo_cmd.arg(&self.command).args(&self.args)
        } else {
            let cmd = Exec::cmd(&self.command).args(&self.args);
            self.apply_env_to_exec(cmd)
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
            info!("{}", m);
        }

        debug!(?cmd);

        if !self.dry {
            if let Some(m) = &self.message {
                cmd.capture().wrap_err(m.clone())?;
            } else {
                cmd.capture()?;
            }
        }

        Ok(())
    }

    pub fn run_capture(&self) -> Result<Option<String>> {
        let cmd = Exec::cmd(&self.command)
            .args(&self.args)
            .stderr(Redirection::None)
            .stdout(Redirection::Pipe);

        let cmd = self.apply_env_to_exec(cmd);

        if let Some(m) = &self.message {
            info!("{}", m);
        }

        debug!(?cmd);

        if self.dry {
            Ok(None)
        } else {
            Ok(Some(cmd.capture()?.stdout_str()))
        }
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

    pub const fn nom(mut self, yes: bool) -> Self {
        self.nom = yes;
        self
    }

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

    pub fn run(&self) -> Result<()> {
        if let Some(m) = &self.message {
            info!("{}", m);
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
