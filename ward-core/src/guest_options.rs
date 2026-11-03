// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest setup options of `ward run` and `ward create`: validation of what a
//! request asks for, and the arguments that hand it to the guest agent.

use crate::pb;
use crate::protocol::{ApiError, CgroupLimits, GuestOptions, Healthcheck, TmpfsMount};

/// Most tmpfs mounts one sandbox takes.
pub const MAX_TMPFS_MOUNTS: usize = 64;
/// Longest mount destination, as Linux's `PATH_MAX`.
const MAX_PATH_LEN: usize = 4096;
/// Guest paths no mount may cover or sit under: the agent needs them.
const RESERVED_ROOTS: [&str; 3] = ["/proc", "/sys", "/dev"];
/// The one place under `/dev` that is Ward's own: `--shm-size` sizes it.
const SHM_PATH: &str = "/dev/shm";

/// Agent argument that mounts the root filesystem read-only.
pub const READ_ONLY_ARG: &str = "--read-only";
/// Agent argument that mounts a tmpfs: `--tmpfs <path> <options>`.
pub const TMPFS_ARG: &str = "--tmpfs";
/// Agent argument that sizes `/dev/shm`: `--shm-size <bytes>`.
pub const SHM_SIZE_ARG: &str = "--shm-size";

/// Mount flags a tmpfs option may switch on or off.
pub const TMPFS_FLAG_OPTIONS: [&str; 19] = [
    "ro",
    "rw",
    "suid",
    "nosuid",
    "dev",
    "nodev",
    "exec",
    "noexec",
    "sync",
    "async",
    "dirsync",
    "atime",
    "noatime",
    "diratime",
    "nodiratime",
    "relatime",
    "norelatime",
    "strictatime",
    "nostrictatime",
];
/// Filesystem data options of a tmpfs, passed to the kernel as given.
const TMPFS_DATA_KEYS: [&str; 5] = ["size", "mode", "nr_inodes", "uid", "gid"];
/// Largest value of `mode=`: the permission and special bits.
const MAX_TMPFS_MODE: u32 = 0o7777;
const OCTAL_RADIX: u32 = 8;

fn invalid(message: String) -> ApiError {
    ApiError::InvalidRequest(message)
}

/// Check one tmpfs option string, such as `size=64m,noexec`.
///
/// # Errors
///
/// Fails on an unknown option or a bad value.
pub fn check_tmpfs_options(options: &str) -> Result<(), String> {
    for option in options.split(',').filter(|o| !o.is_empty()) {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        if option.contains('=') {
            check_tmpfs_data(key, value)?;
        } else if !TMPFS_FLAG_OPTIONS.contains(&key) {
            return Err(format!("invalid tmpfs option '{option}'"));
        }
    }
    Ok(())
}

fn check_tmpfs_data(key: &str, value: &str) -> Result<(), String> {
    let ok = match key {
        "size" | "nr_inodes" => size_in_bytes(value).is_some(),
        "mode" => u32::from_str_radix(value, OCTAL_RADIX).is_ok_and(|m| m <= MAX_TMPFS_MODE),
        "uid" | "gid" => value.parse::<u32>().is_ok(),
        _ => {
            return Err(format!(
                "invalid tmpfs option '{key}': use one of {}",
                TMPFS_DATA_KEYS.join(", ")
            ));
        }
    };
    if ok {
        Ok(())
    } else {
        Err(format!("invalid value '{value}' for tmpfs option '{key}'"))
    }
}

/// A tmpfs size such as `65536`, `64k`, `64m` or `1g`, in bytes. `None` when
/// the text is not that or does not fit 64 bits.
#[must_use]
pub fn size_in_bytes(text: &str) -> Option<u64> {
    let (digits, unit) = match text.char_indices().last()? {
        (i, 'k' | 'K') => (&text[..i], 1u64 << 10),
        (i, 'm' | 'M') => (&text[..i], 1u64 << 20),
        (i, 'g' | 'G') => (&text[..i], 1u64 << 30),
        _ => (text, 1),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(unit)
}

/// Normalise a guest path: absolute, no `.` or `..`, no empty parts.
fn clean_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/') || path.len() > MAX_PATH_LEN || path.contains('\0') {
        return Err(format!(
            "mount destination must be an absolute path: {path}"
        ));
    }
    let mut parts = Vec::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        if part == ".." || part == "." {
            return Err(format!(
                "mount destination must not contain '{part}': {path}"
            ));
        }
        parts.push(part);
    }
    Ok(format!("/{}", parts.join("/")))
}

fn is_reserved(path: &str) -> bool {
    path == SHM_PATH
        || RESERVED_ROOTS
            .iter()
            .any(|root| path == *root || path.starts_with(&format!("{root}/")))
}

/// Check a request's guest options against the mounts it also asks for, and
/// return them as stored on the sandbox.
///
/// # Errors
///
/// Fails on a bad tmpfs path or option, a destination used twice, or one in
/// a place the guest reserves.
pub fn validate(
    guest: Option<&pb::GuestOptions>,
    mounts: &[pb::Mount],
    memory_mb: u32,
) -> Result<GuestOptions, ApiError> {
    let Some(guest) = guest else {
        return Ok(GuestOptions::default());
    };
    if guest.tmpfs.len() > MAX_TMPFS_MOUNTS {
        return Err(invalid(format!(
            "at most {MAX_TMPFS_MOUNTS} tmpfs mounts are allowed"
        )));
    }
    let mut seen = Vec::with_capacity(mounts.len() + guest.tmpfs.len());
    for mount in mounts {
        seen.push(clean_path(&mount.target).map_err(invalid)?);
    }
    let mut tmpfs = Vec::with_capacity(guest.tmpfs.len());
    for entry in &guest.tmpfs {
        let path = clean_path(&entry.path).map_err(invalid)?;
        if path == "/" || is_reserved(&path) {
            return Err(invalid(format!(
                "mount destination {path} is reserved by the guest; /dev/shm is sized with --shm-size"
            )));
        }
        check_tmpfs_options(&entry.options).map_err(invalid)?;
        if seen.contains(&path) {
            return Err(invalid(format!("Duplicate mount point: {path}")));
        }
        seen.push(path.clone());
        tmpfs.push(TmpfsMount {
            path,
            options: entry.options.clone(),
        });
    }
    crate::guest_network::search_domains(&guest.dns_search)?;
    crate::guest_network::dns_options(&guest.dns_options)?;
    let cgroup = guest.cgroup.clone().unwrap_or_default();
    let cgroup = CgroupLimits {
        memory_swap_bytes: cgroup.memory_swap_bytes,
        memory_reservation_bytes: cgroup.memory_reservation_bytes,
        cpu_shares: cgroup.cpu_shares,
        cpuset_cpus: cgroup.cpuset_cpus,
        cpu_period: cgroup.cpu_period,
        cpu_quota: cgroup.cpu_quota,
    };
    crate::guest_limits::cgroup(&cgroup, memory_mb)?;
    crate::guest_limits::group_add(&guest.group_add)?;
    let healthcheck = guest
        .healthcheck
        .as_ref()
        .map(|h| {
            let config = Healthcheck {
                test: h.test.clone(),
                interval_ns: h.interval_ns,
                timeout_ns: h.timeout_ns,
                start_period_ns: h.start_period_ns,
                retries: h.retries,
            };
            crate::sandbox::health::validate(&config).map(|()| config)
        })
        .transpose()?;
    Ok(GuestOptions {
        healthcheck,
        ulimits: crate::guest_limits::ulimits(&guest.ulimits)?,
        group_add: guest.group_add.clone(),
        cgroup,
        read_only_rootfs: guest.read_only_rootfs,
        tmpfs,
        shm_size_bytes: guest.shm_size_bytes,
        extra_hosts: crate::guest_network::extra_hosts(&guest.extra_hosts)?,
        dns_search: guest.dns_search.clone(),
        dns_options: guest.dns_options.clone(),
    })
}

/// The options as the wire carries them.
#[must_use]
pub fn to_pb(guest: &GuestOptions) -> pb::GuestOptions {
    pb::GuestOptions {
        read_only_rootfs: guest.read_only_rootfs,
        tmpfs: guest
            .tmpfs
            .iter()
            .map(|t| pb::TmpfsMount {
                path: t.path.clone(),
                options: t.options.clone(),
            })
            .collect(),
        shm_size_bytes: guest.shm_size(),
        extra_hosts: guest.extra_hosts.clone(),
        dns_search: guest.dns_search.clone(),
        dns_options: guest.dns_options.clone(),
        ulimits: guest
            .ulimits
            .iter()
            .map(|u| pb::Ulimit {
                name: u.name.clone(),
                soft: u.soft,
                hard: u.hard,
            })
            .collect(),
        group_add: guest.group_add.clone(),
        healthcheck: guest.healthcheck.as_ref().map(|h| pb::Healthcheck {
            test: h.test.clone(),
            interval_ns: h.interval_ns,
            timeout_ns: h.timeout_ns,
            start_period_ns: h.start_period_ns,
            retries: h.retries,
        }),
        cgroup: Some(pb::CgroupLimits {
            memory_swap_bytes: guest.cgroup.memory_swap_bytes,
            memory_reservation_bytes: guest.cgroup.memory_reservation_bytes,
            cpu_shares: guest.cgroup.cpu_shares,
            cpuset_cpus: guest.cgroup.cpuset_cpus.clone(),
            cpu_period: guest.cgroup.cpu_period,
            cpu_quota: guest.cgroup.cpu_quota,
        }),
    }
}

/// The guest agent arguments that apply `guest` at boot.
#[must_use]
pub fn agent_args(guest: &GuestOptions) -> Vec<String> {
    let mut args = Vec::new();
    if guest.read_only_rootfs {
        args.push(READ_ONLY_ARG.to_string());
    }
    for mount in &guest.tmpfs {
        args.extend([
            TMPFS_ARG.to_string(),
            mount.path.clone(),
            mount.options.clone(),
        ]);
    }
    args.extend([SHM_SIZE_ARG.to_string(), guest.shm_size().to_string()]);
    args.extend(crate::guest_network::agent_args(
        &guest.extra_hosts,
        &guest.dns_search,
        &guest.dns_options,
    ));
    args.extend(crate::guest_limits::agent_args(
        &guest.ulimits,
        &guest.group_add,
        &guest.cgroup,
    ));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHM_BYTES: u64 = 1 << 20;

    fn tmpfs(path: &str, options: &str) -> pb::TmpfsMount {
        pb::TmpfsMount {
            path: path.into(),
            options: options.into(),
        }
    }

    fn guest(tmpfs: Vec<pb::TmpfsMount>) -> pb::GuestOptions {
        pb::GuestOptions {
            tmpfs,
            ..Default::default()
        }
    }

    #[rstest::rstest]
    #[case::empty("", true)]
    #[case::flags("rw,noexec,nosuid", true)]
    #[case::size_and_mode("size=64m,mode=1777", true)]
    #[case::bytes("size=65536", true)]
    #[case::ids("uid=1000,gid=1000,nr_inodes=1k", true)]
    #[case::unknown_flag("bogus", false)]
    #[case::unknown_key("color=red", false)]
    #[case::bad_size("size=big", false)]
    #[case::empty_size("size=", false)]
    #[case::overflow("size=99999999999g", false)]
    #[case::mode_not_octal("mode=999", false)]
    #[case::mode_too_big("mode=17777", false)]
    #[case::remount("remount", false)]
    fn tmpfs_options_accept_the_documented_vocabulary_only(
        #[case] options: &str,
        #[case] ok: bool,
    ) {
        // Act
        let result = check_tmpfs_options(options);

        // Assert
        assert_eq!(result.is_ok(), ok, "{result:?}");
    }

    #[rstest::rstest]
    #[case("65536", Some(65_536))]
    #[case("64k", Some(65_536))]
    #[case("64M", Some(64 << 20))]
    #[case("2g", Some(2 << 30))]
    #[case("", None)]
    #[case("k", None)]
    #[case("1.5m", None)]
    #[case("-1", None)]
    fn size_is_a_count_with_an_optional_k_m_or_g(#[case] text: &str, #[case] want: Option<u64>) {
        assert_eq!(size_in_bytes(text), want);
    }

    #[test]
    fn validate_cleans_paths_and_keeps_options() {
        // Arrange
        let request = guest(vec![tmpfs("/var//cache/", "size=1m")]);

        // Act
        let stored = validate(Some(&request), &[], 0).unwrap();

        // Assert
        assert_eq!(stored.tmpfs[0].path, "/var/cache");
        assert_eq!(stored.tmpfs[0].options, "size=1m");
    }

    #[rstest::rstest]
    #[case::relative("run")]
    #[case::root("/")]
    #[case::parent("/a/../b")]
    #[case::proc("/proc")]
    #[case::under_sys("/sys/fs")]
    #[case::dev("/dev")]
    #[case::shm("/dev/shm")]
    fn validate_refuses_paths_the_guest_cannot_give(#[case] path: &str) {
        // Arrange
        let request = guest(vec![tmpfs(path, "")]);

        // Act
        let result = validate(Some(&request), &[], 0);

        // Assert
        assert!(result.is_err(), "{path}");
    }

    #[test]
    fn validate_refuses_a_destination_used_twice_across_kinds() {
        // Arrange
        let request = guest(vec![tmpfs("/data", ""), tmpfs("/other", "")]);
        let bind = pb::Mount {
            source: "/tmp/x".into(),
            target: "/data/".into(),
            readonly: false,
        };

        // Act
        let error = validate(Some(&request), &[bind], 0).unwrap_err();

        // Assert
        assert!(error.to_string().contains("Duplicate mount point: /data"));
    }

    #[test]
    fn validate_refuses_too_many_tmpfs_mounts() {
        // Arrange
        let many = (0..=MAX_TMPFS_MOUNTS)
            .map(|i| tmpfs(&format!("/t{i}"), ""))
            .collect();

        // Act
        let result = validate(Some(&guest(many)), &[], 0);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn agent_args_always_size_shm_and_list_the_rest_in_order() {
        // Arrange
        let options = GuestOptions {
            read_only_rootfs: true,
            tmpfs: vec![TmpfsMount {
                path: "/run".into(),
                options: "size=1m".into(),
            }],
            shm_size_bytes: SHM_BYTES,
            ..Default::default()
        };

        // Act
        let args = agent_args(&options);

        // Assert
        assert_eq!(
            args,
            [
                "--read-only",
                "--tmpfs",
                "/run",
                "size=1m",
                "--shm-size",
                "1048576"
            ]
        );
        assert_eq!(
            agent_args(&GuestOptions::default()),
            ["--shm-size", "67108864"]
        );
    }

    #[test]
    fn to_pb_reports_the_effective_shm_size() {
        assert_eq!(
            to_pb(&GuestOptions::default()).shm_size_bytes,
            crate::protocol::DEFAULT_SHM_SIZE_BYTES
        );
    }
}
