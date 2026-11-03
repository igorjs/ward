// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest setup flags of `ward run` and `ward create`: the read-only root
//! filesystem, tmpfs mounts, `/dev/shm` and `--mount`.

use anyhow::{Result, bail};
use clap::Args;
use ward_core::guest_options::check_tmpfs_options;
use ward_core::pb::{CreateSandboxRequest, GuestOptions, Mount, TmpfsMount};

use crate::resources::parse_size_bytes;

const OPTION_SEPARATOR: char = ',';
const KEY_VALUE_SEPARATOR: char = '=';
const TMPFS_OPTIONS_SEPARATOR: char = ':';
const TYPE_BIND: &str = "bind";
const TYPE_TMPFS: &str = "tmpfs";
const TYPE_VOLUME: &str = "volume";

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
    /// Mount with Docker's key=value syntax: type=bind|tmpfs, source= (or
    /// src=), destination= (dst=, target=), readonly (ro), tmpfs-size= and
    /// tmpfs-mode=. Named volumes cannot be attached yet. Repeatable.
    #[arg(long, value_name = "type=TYPE,src=SRC,dst=DST[,ro]")]
    mount: Vec<String>,
}

impl GuestExtras {
    /// Put the guest options and the `--mount` binds on `request`.
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
        for spec in &self.mount {
            match parse_mount(spec)? {
                MountArg::Bind(mount) => request.mounts.push(mount),
                MountArg::Tmpfs(tmpfs) => guest.tmpfs.push(tmpfs),
            }
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

/// What one `--mount` stands for.
#[derive(Debug, PartialEq)]
enum MountArg {
    Bind(Mount),
    Tmpfs(TmpfsMount),
}

/// Parse the key=value list of `--mount` onto the `-v` and `--tmpfs` support.
fn parse_mount(spec: &str) -> Result<MountArg> {
    let mut kind = TYPE_VOLUME;
    let (mut source, mut target, mut readonly) = (None, None, false);
    let (mut size, mut mode) = (None, None);
    for field in spec.split(OPTION_SEPARATOR).filter(|f| !f.is_empty()) {
        let (key, value) = field
            .split_once(KEY_VALUE_SEPARATOR)
            .map_or((field, None), |(k, v)| (k, Some(v)));
        match (key, value) {
            ("type", Some(value)) => kind = value,
            ("source" | "src", Some(value)) => source = Some(value),
            ("destination" | "dst" | "target", Some(value)) => target = Some(value),
            ("readonly" | "ro", value) => readonly = parse_flag(key, value)?,
            ("tmpfs-size", Some(value)) => size = Some(parse_size_bytes(value)?),
            ("tmpfs-mode", Some(value)) => mode = Some(value),
            _ => bail!("--mount {spec}: unsupported option '{field}'"),
        }
    }
    let Some(target) = target else {
        bail!("--mount {spec}: destination is required");
    };
    match kind {
        TYPE_BIND => {
            let Some(source) = source else {
                bail!("--mount {spec}: type=bind needs source=");
            };
            if size.is_some() || mode.is_some() {
                bail!("--mount {spec}: tmpfs options need type=tmpfs");
            }
            Ok(MountArg::Bind(Mount {
                source: source.to_string(),
                target: target.to_string(),
                readonly,
            }))
        }
        TYPE_TMPFS => {
            if source.is_some() {
                bail!("--mount {spec}: type=tmpfs has no source");
            }
            let mut options = vec![if readonly { "ro" } else { "rw" }.to_string()];
            if let Some(size) = size {
                options.push(format!("size={size}"));
            }
            if let Some(mode) = mode {
                options.push(format!("mode={mode}"));
            }
            let options = options.join(&OPTION_SEPARATOR.to_string());
            if let Err(reason) = check_tmpfs_options(&options) {
                bail!("--mount {spec}: {reason}");
            }
            Ok(MountArg::Tmpfs(TmpfsMount {
                path: target.to_string(),
                options,
            }))
        }
        TYPE_VOLUME => bail!(
            "--mount {spec}: named volumes cannot be attached to a sandbox yet; use type=bind or type=tmpfs"
        ),
        other => bail!("--mount {spec}: unsupported type '{other}', use bind or tmpfs"),
    }
}

/// A Docker boolean option: bare, or `true`/`false` and `1`/`0`.
fn parse_flag(key: &str, value: Option<&str>) -> Result<bool> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(other) => bail!("invalid value '{other}' for --mount option '{key}'"),
    }
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
    fn given_bind_mount_when_parsed_then_it_maps_onto_a_volume_flag_mount() {
        // Act
        let parsed = parse_mount("type=bind,src=/tmp/a,dst=/data,readonly").unwrap();

        // Assert
        assert_eq!(
            parsed,
            MountArg::Bind(Mount {
                source: "/tmp/a".into(),
                target: "/data".into(),
                readonly: true
            })
        );
    }

    #[test]
    fn given_tmpfs_mount_when_parsed_then_size_and_mode_become_tmpfs_options() {
        // Act
        let parsed =
            parse_mount("type=tmpfs,target=/cache,tmpfs-size=64m,tmpfs-mode=1770").unwrap();

        // Assert
        assert_eq!(
            parsed,
            MountArg::Tmpfs(TmpfsMount {
                path: "/cache".into(),
                options: "rw,size=67108864,mode=1770".into()
            })
        );
    }

    #[rstest::rstest]
    #[case::volume("type=volume,src=data,dst=/data", "named volumes")]
    #[case::default_type_is_volume("src=data,dst=/data", "named volumes")]
    #[case::unknown_type("type=npipe,dst=/x", "unsupported type")]
    #[case::unknown_key(
        "type=bind,src=/a,dst=/b,bind-propagation=rshared",
        "unsupported option"
    )]
    #[case::no_destination("type=bind,src=/a", "destination is required")]
    #[case::bind_without_source("type=bind,dst=/b", "needs source")]
    #[case::tmpfs_with_source("type=tmpfs,src=/a,dst=/b", "no source")]
    #[case::size_on_bind("type=bind,src=/a,dst=/b,tmpfs-size=1m", "need type=tmpfs")]
    #[case::bad_mode("type=tmpfs,dst=/b,tmpfs-mode=999", "mode")]
    #[case::bad_readonly("type=bind,src=/a,dst=/b,readonly=maybe", "invalid value")]
    fn given_unsupported_mount_when_parsed_then_the_reason_is_clear(
        #[case] spec: &str,
        #[case] reason: &str,
    ) {
        // Act
        let error = parse_mount(spec).unwrap_err();

        // Assert
        assert!(error.to_string().contains(reason), "{error}");
    }

    #[test]
    fn given_flags_when_applied_then_request_carries_guest_options_and_binds() {
        // Arrange
        let extras = GuestExtras {
            read_only: true,
            tmpfs: vec!["/run:size=1m".into()],
            shm_size: Some("128m".into()),
            mount: vec![
                "type=bind,src=/tmp/a,dst=/data".into(),
                "type=tmpfs,dst=/cache".into(),
            ],
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
            ["/run", "/cache"]
        );
        assert_eq!(request.mounts.len(), 1);
    }
}
