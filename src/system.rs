// Copyright 2024 nh contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Interface to `system-manager`.

use color_eyre::eyre::{Context, Result, bail};
use subprocess::{Exec, ExitStatus};
use tracing::{debug, instrument};

use crate::interface::{SystemBuildArgs, SystemRollbackArgs};

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

fn ensure_system_manager() -> Result<String> {
    which::which("system-manager")
        .map(|p| p.to_string_lossy().into_owned())
        .wrap_err("`system-manager` not found in $PATH")
}

pub struct SystemManager;

impl SystemManager {
    #[instrument(level = "debug", skip(args))]
    pub fn build(args: &SystemBuildArgs) -> Result<()> {
        if cfg!(target_os = "macos") {
            bail!("system-manager is Linux-only");
        }

        let bin = ensure_system_manager()?;
        let mut cmd = Exec::cmd(bin).arg("build");

        if let Some(flake) = &args.flake {
            cmd = cmd.arg("--flake").arg(flake);
        }
        if args.switch {
            cmd = cmd.arg("--switch");
        }
        if args.dry_activate {
            cmd = cmd.arg("--dry-activate");
        }
        if args.no_link {
            cmd = cmd.arg("--no-link");
        }

        cmd = cmd.args(&args.passthrough.generate_passthrough_args());
        cmd = ssh_wrap(cmd, args.install_host.as_deref());
        cmd = ssh_wrap(cmd, args.ssh.as_deref());
        debug!(?cmd);
        let status = cmd.join()?;
        match status {
            ExitStatus::Exited(0) => Ok(()),
            other => bail!("system-manager failed: {:?}", other),
        }
    }

    #[instrument(level = "debug")]
    pub fn list_generations(ssh: Option<&str>) -> Result<()> {
        if cfg!(target_os = "macos") {
            bail!("system-manager is Linux-only");
        }

        let bin = ensure_system_manager()?;
        let mut cmd = Exec::cmd(bin).arg("list-generations");
        cmd = ssh_wrap(cmd, ssh);
        debug!(?cmd);
        let status = cmd.join()?;
        match status {
            ExitStatus::Exited(0) => Ok(()),
            other => bail!("system-manager failed: {:?}", other),
        }
    }

    #[instrument(level = "debug", skip(args))]
    pub fn rollback(args: &SystemRollbackArgs) -> Result<()> {
        if cfg!(target_os = "macos") {
            bail!("system-manager is Linux-only");
        }

        let bin = ensure_system_manager()?;
        let mut cmd = Exec::cmd(bin).arg("rollback");
        if let Some(generation) = &args.generation {
            cmd = cmd.arg(generation);
        }
        cmd = ssh_wrap(cmd, args.install_host.as_deref());
        cmd = ssh_wrap(cmd, args.ssh.as_deref());
        debug!(?cmd);
        let status = cmd.join()?;
        match status {
            ExitStatus::Exited(0) => Ok(()),
            other => bail!("system-manager failed: {:?}", other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_system_build_args_parse() {
        let args =
            SystemBuildArgs::parse_from(["nh", "--flake", "flake", "--switch", "--dry-activate"]);
        assert_eq!(args.flake.as_deref(), Some("flake"));
        assert!(args.switch);
        assert!(args.dry_activate);
    }
}
