// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Docker flags that change host isolation. A microVM has no use for them, so
//! the parser accepts them only to refuse them with a reason.

use anyhow::{Result, bail};
use clap::Args;

const DOCS: &str = "docs/docker-differences.md";

const NO_HOST_PRIVILEGES: &str =
    "the sandbox is a separate VM with its own kernel, so there are no host privileges to grant";
const NO_CAPABILITY_CHANGES: &str = "guest processes run with the guest kernel's own capabilities, which cannot be changed from outside";
const NO_SECURITY_PROFILES: &str =
    "seccomp, AppArmor and SELinux profiles guard a host kernel, and the sandbox has its own";
const NO_DEVICES: &str = "host devices cannot be passed through to a microVM guest";
const USE_EGRESS: &str = "network access is set with --egress";
const NO_HOST_NAMESPACES: &str =
    "the sandbox runs its own kernel, so it has its own namespaces and cannot join the host's";

/// Isolation flags of `docker run`. Hidden from help; each one is an error.
#[derive(Debug, Args, Default)]
pub struct IsolationFlags {
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    privileged: Option<bool>,
    #[arg(long = "cap-add", hide = true, value_name = "CAP")]
    cap_add: Vec<String>,
    #[arg(long = "cap-drop", hide = true, value_name = "CAP")]
    cap_drop: Vec<String>,
    #[arg(long = "security-opt", hide = true, value_name = "OPT")]
    security_opt: Vec<String>,
    #[arg(long, hide = true, value_name = "DEVICE")]
    device: Vec<String>,
    #[arg(long, alias = "net", hide = true, value_name = "NETWORK")]
    network: Option<String>,
    #[arg(long, hide = true, value_name = "MODE")]
    ipc: Option<String>,
    #[arg(long, hide = true, value_name = "MODE")]
    pid: Option<String>,
    #[arg(long, hide = true, value_name = "MODE")]
    uts: Option<String>,
    #[arg(long, hide = true, value_name = "MODE")]
    userns: Option<String>,
    #[arg(long, hide = true, value_name = "MODE")]
    cgroupns: Option<String>,
}

impl IsolationFlags {
    /// Refuse the first isolation flag that was given.
    ///
    /// # Errors
    ///
    /// Fails with the flag, why a microVM has no use for it, and where to
    /// read more.
    pub fn check(&self) -> Result<()> {
        let given = [
            (
                "--privileged",
                self.privileged == Some(true),
                NO_HOST_PRIVILEGES,
            ),
            ("--cap-add", !self.cap_add.is_empty(), NO_CAPABILITY_CHANGES),
            (
                "--cap-drop",
                !self.cap_drop.is_empty(),
                NO_CAPABILITY_CHANGES,
            ),
            (
                "--security-opt",
                !self.security_opt.is_empty(),
                NO_SECURITY_PROFILES,
            ),
            ("--device", !self.device.is_empty(), NO_DEVICES),
            ("--network", self.network.is_some(), USE_EGRESS),
            ("--ipc", self.ipc.is_some(), NO_HOST_NAMESPACES),
            ("--pid", self.pid.is_some(), NO_HOST_NAMESPACES),
            ("--uts", self.uts.is_some(), NO_HOST_NAMESPACES),
            ("--userns", self.userns.is_some(), NO_HOST_NAMESPACES),
            ("--cgroupns", self.cgroupns.is_some(), NO_HOST_NAMESPACES),
        ];
        match given.into_iter().find(|(_, set, _)| *set) {
            Some((flag, _, reason)) => bail!("{flag} is not supported: {reason}. See {DOCS}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_no_flags_when_checked_then_it_passes() {
        assert!(IsolationFlags::default().check().is_ok());
    }

    #[test]
    fn given_privileged_false_when_checked_then_it_passes_as_docker_ignores_it() {
        let flags = IsolationFlags {
            privileged: Some(false),
            ..Default::default()
        };

        assert!(flags.check().is_ok());
    }

    #[rstest::rstest]
    #[case::privileged(IsolationFlags { privileged: Some(true), ..Default::default() }, "--privileged")]
    #[case::cap_add(IsolationFlags { cap_add: vec!["NET_ADMIN".into()], ..Default::default() }, "--cap-add")]
    #[case::cap_drop(IsolationFlags { cap_drop: vec!["ALL".into()], ..Default::default() }, "--cap-drop")]
    #[case::security_opt(IsolationFlags { security_opt: vec!["seccomp=unconfined".into()], ..Default::default() }, "--security-opt")]
    #[case::device(IsolationFlags { device: vec!["/dev/fuse".into()], ..Default::default() }, "--device")]
    #[case::network(IsolationFlags { network: Some("host".into()), ..Default::default() }, "--network")]
    #[case::ipc(IsolationFlags { ipc: Some("host".into()), ..Default::default() }, "--ipc")]
    #[case::pid(IsolationFlags { pid: Some("host".into()), ..Default::default() }, "--pid")]
    #[case::uts(IsolationFlags { uts: Some("host".into()), ..Default::default() }, "--uts")]
    #[case::userns(IsolationFlags { userns: Some("host".into()), ..Default::default() }, "--userns")]
    #[case::cgroupns(IsolationFlags { cgroupns: Some("host".into()), ..Default::default() }, "--cgroupns")]
    fn given_an_isolation_flag_when_checked_then_the_error_names_it_and_the_docs(
        #[case] flags: IsolationFlags,
        #[case] name: &str,
    ) {
        // Act
        let error = flags.check().unwrap_err().to_string();

        // Assert
        assert!(error.starts_with(name), "{error}");
        assert!(error.ends_with(DOCS), "{error}");
        assert_eq!(error.lines().count(), 1);
    }
}
