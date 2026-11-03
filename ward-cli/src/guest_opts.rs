// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest setup flags of `ward run` and `ward create`: the read-only root
//! filesystem, tmpfs mounts and `/dev/shm`.

use anyhow::{Result, bail};
use clap::Args;
use ward_core::guest_options::check_tmpfs_options;
use ward_core::pb::{CreateSandboxRequest, GuestOptions, TmpfsMount};

use crate::resources::parse_size_bytes;

const TMPFS_OPTIONS_SEPARATOR: char = ':';

/// Guest setup flags that `create` and `run` share.
#[derive(Debug, Args, Default)]
pub struct GuestExtras {
    /// Mount the root filesystem read-only. /tmp and /run get a writable
    /// tmpfs, as do /dev and /dev/shm; --tmpfs mounts and -v mounts stay
    /// writable.
    #[arg(long = "read-only")]
    read_only: bool,
    /// Mount a tmpfs at PATH, with options such as size=64m, mode=1777, ro or
    /// exec. Defaults to rw,noexec,nosuid,nodev. Repeatable.
    #[arg(long, value_name = "PATH[:OPTIONS]")]
    tmpfs: Vec<String>,
    /// Size of /dev/shm: bytes, or a number with a k, m, g or t suffix.
    /// Default 64m.
    #[arg(long = "shm-size", value_name = "SIZE")]
    shm_size: Option<String>,
}

impl GuestExtras {
    /// Put the guest options on `request`.
    ///
    /// # Errors
    ///
    /// Fails on a malformed or unsupported value.
    pub fn apply(&self, request: &mut CreateSandboxRequest) -> Result<()> {
        let mut guest = GuestOptions {
            read_only_rootfs: self.read_only,
            ..GuestOptions::default()
        };
        for spec in &self.tmpfs {
            guest.tmpfs.push(parse_tmpfs(spec)?);
        }
        if let Some(size) = &self.shm_size {
            guest.shm_size_bytes = parse_size_bytes(size)?;
        }
        request.guest = Some(guest);
        Ok(())
    }
}

/// Parse `PATH[:OPTIONS]` of `--tmpfs`.
fn parse_tmpfs(spec: &str) -> Result<TmpfsMount> {
    let (path, options) = spec
        .split_once(TMPFS_OPTIONS_SEPARATOR)
        .unwrap_or((spec, ""));
    if path.is_empty() {
        bail!("--tmpfs expects PATH[:OPTIONS], got '{spec}'");
    }
    if let Err(reason) = check_tmpfs_options(options) {
        bail!("--tmpfs {spec}: {reason}");
    }
    Ok(TmpfsMount {
        path: path.to_string(),
        options: options.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::bare("/scratch", "/scratch", "")]
    #[case::options("/run:size=64m,noexec", "/run", "size=64m,noexec")]
    fn given_tmpfs_flag_when_parsed_then_path_and_options_split(
        #[case] spec: &str,
        #[case] path: &str,
        #[case] options: &str,
    ) {
        // Act
        let parsed = parse_tmpfs(spec).unwrap();

        // Assert
        assert_eq!(
            (parsed.path.as_str(), parsed.options.as_str()),
            (path, options)
        );
    }

    #[rstest::rstest]
    #[case::no_path(":size=1m")]
    #[case::bad_option("/x:bogus")]
    #[case::bad_size("/x:size=lots")]
    fn given_bad_tmpfs_flag_when_parsed_then_it_names_the_flag(#[case] spec: &str) {
        // Act
        let error = parse_tmpfs(spec).unwrap_err();

        // Assert
        assert!(error.to_string().contains("--tmpfs"), "{error}");
    }

    #[test]
    fn given_flags_when_applied_then_request_carries_guest_options() {
        // Arrange
        let extras = GuestExtras {
            read_only: true,
            tmpfs: vec!["/run:size=1m".into()],
            shm_size: Some("128m".into()),
        };
        let mut request = CreateSandboxRequest::default();

        // Act
        extras.apply(&mut request).unwrap();

        // Assert
        let guest = request.guest.unwrap();
        assert!(guest.read_only_rootfs);
        assert_eq!(guest.shm_size_bytes, 128 << 20);
        assert_eq!(
            guest
                .tmpfs
                .iter()
                .map(|t| t.path.as_str())
                .collect::<Vec<_>>(),
            ["/run"]
        );
    }
}
