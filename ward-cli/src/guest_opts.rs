// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest setup flags of `ward run` and `ward create`: the read-only root
//! filesystem, tmpfs mounts, `/dev/shm` and `--mount`.

use anyhow::{Result, bail};
use clap::Args;
use ward_core::guest_options::check_tmpfs_options;
use ward_core::pb::{
    CgroupLimits, CreateSandboxRequest, GuestOptions, Healthcheck, Mount, TmpfsMount, Ulimit,
};

use crate::resources::parse_size_bytes;
use crate::volume;

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
    /// Mount with Docker's key=value syntax: type=bind|volume|tmpfs, source= (or
    /// src=), destination= (dst=, target=), readonly (ro), tmpfs-size= and
    /// tmpfs-mode=. Repeatable.
    #[arg(long, value_name = "type=TYPE,src=SRC,dst=DST[,ro]")]
    mount: Vec<String>,
    /// Add a line to /etc/hosts: HOST:IP. HOST:host-gateway is the host
    /// stack's address, reachable only while the sandbox has a network
    /// device. Repeatable.
    #[arg(long = "add-host", value_name = "HOST:IP")]
    add_host: Vec<String>,
    /// Not supported: the sandbox's resolver is the host relay that enforces
    /// --egress, and no other server is reachable.
    #[arg(long, hide = true, value_name = "IP")]
    dns: Vec<String>,
    /// Add a search domain to /etc/resolv.conf. Repeatable.
    #[arg(long = "dns-search", value_name = "DOMAIN")]
    dns_search: Vec<String>,
    /// Add a resolver option such as ndots:2 to /etc/resolv.conf. Repeatable.
    #[arg(long = "dns-option", value_name = "OPTION")]
    dns_option: Vec<String>,
    /// Set a resource limit on every process: NAME=SOFT[:HARD], where -1 is
    /// unlimited. Names: core, cpu, data, fsize, locks, memlock, msgqueue,
    /// nice, nofile, nproc, rss, rtprio, rttime, sigpending, stack. Repeatable.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    ulimit: Vec<String>,
    /// Add a supplementary group, by name or id, to every process. Repeatable.
    #[arg(long = "group-add", value_name = "GROUP|GID")]
    group_add: Vec<String>,
    /// Only a value equal to --memory is accepted: the guest has no swap.
    #[arg(long = "memory-swap", value_name = "SIZE", allow_hyphen_values = true)]
    memory_swap: Option<String>,
    /// Memory the guest cgroup protects from reclaim (memory.low). Must not
    /// exceed --memory.
    #[arg(long = "memory-reservation", value_name = "SIZE")]
    memory_reservation: Option<String>,
    /// Relative CPU weight, 2 to 262144 (default 1024), as cgroup v2 cpu.weight.
    #[arg(long = "cpu-shares", value_name = "N")]
    cpu_shares: Option<u32>,
    /// CPUs the processes may use, such as 0-1,3 (cpuset.cpus).
    #[arg(long = "cpuset-cpus", value_name = "LIST")]
    cpuset_cpus: Option<String>,
    /// Length of a CPU scheduling period in microseconds, 1000 to 1000000.
    #[arg(long = "cpu-period", value_name = "MICROSECONDS")]
    cpu_period: Option<u64>,
    /// CPU time per period in microseconds, at least 1000; -1 or 0 for none.
    #[arg(
        long = "cpu-quota",
        value_name = "MICROSECONDS",
        allow_negative_numbers = true
    )]
    cpu_quota: Option<i64>,
    /// Command the daemon runs in the sandbox to check its health, under
    /// /bin/sh -c. Exit 0 is healthy.
    #[arg(long = "health-cmd", value_name = "COMMAND")]
    health_cmd: Option<String>,
    /// Time between checks, such as 30s or 1m (default 30s).
    #[arg(long = "health-interval", value_name = "DURATION", value_parser = parse_health_duration)]
    health_interval: Option<std::time::Duration>,
    /// Time one check may take before it counts as failed (default 30s).
    #[arg(long = "health-timeout", value_name = "DURATION", value_parser = parse_health_duration)]
    health_timeout: Option<std::time::Duration>,
    /// Failed checks in a row that make the sandbox unhealthy (default 3).
    #[arg(long = "health-retries", value_name = "N")]
    health_retries: Option<u32>,
    /// Time after start in which failed checks do not count (default 0s).
    #[arg(long = "health-start-period", value_name = "DURATION", value_parser = parse_health_duration)]
    health_start_period: Option<std::time::Duration>,
    /// Run no health check.
    #[arg(long = "no-healthcheck")]
    no_healthcheck: bool,
}

const HEALTH_SHELL: &str = "CMD-SHELL";
const HEALTH_NONE: &str = "NONE";

fn parse_health_duration(value: &str) -> Result<std::time::Duration, String> {
    crate::log_output::parse_duration(value)
        .ok_or_else(|| format!("invalid duration '{value}': use a value like 30s, 1m or 1h30m"))
}

const ULIMIT_SEPARATOR: char = '=';
const ULIMIT_HARD_SEPARATOR: char = ':';
/// The value that means unlimited, for sizes and ulimits.
const UNLIMITED: i64 = -1;

const DNS_REFUSAL: &str = "--dns is not supported: the sandbox resolves names through the host relay that enforces --egress, and no other name server is reachable. See docs/docker-differences.md";

impl GuestExtras {
    /// Put the guest options and the `--mount` binds on `request`.
    ///
    /// # Errors
    ///
    /// Fails on a malformed or unsupported value.
    pub fn apply(&self, request: &mut CreateSandboxRequest) -> Result<()> {
        if !self.dns.is_empty() {
            bail!(DNS_REFUSAL);
        }
        let mut guest = GuestOptions {
            read_only_rootfs: self.read_only,
            extra_hosts: self.add_host.clone(),
            dns_search: self.dns_search.clone(),
            dns_options: self.dns_option.clone(),
            ulimits: self
                .ulimit
                .iter()
                .map(|spec| parse_ulimit(spec))
                .collect::<Result<_>>()?,
            group_add: self.group_add.clone(),
            cgroup: Some(self.cgroup_limits()?),
            healthcheck: self.healthcheck()?,
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

impl GuestExtras {
    fn healthcheck(&self) -> Result<Option<Healthcheck>> {
        let tuned = self.health_interval.is_some()
            || self.health_timeout.is_some()
            || self.health_start_period.is_some()
            || self.health_retries.is_some();
        match (&self.health_cmd, self.no_healthcheck) {
            (Some(_), true) => bail!("conflicting options: --no-healthcheck and --health-cmd"),
            (None, true) if tuned => bail!("conflicting options: --no-healthcheck and --health-*"),
            (None, true) => Ok(Some(Healthcheck {
                test: vec![HEALTH_NONE.to_string()],
                ..Default::default()
            })),
            (None, false) if tuned => bail!("--health-* options need --health-cmd"),
            (None, false) => Ok(None),
            (Some(command), false) => {
                let nanos = |d: Option<std::time::Duration>| {
                    d.map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
                };
                Ok(Some(Healthcheck {
                    test: vec![HEALTH_SHELL.to_string(), command.clone()],
                    interval_ns: nanos(self.health_interval),
                    timeout_ns: nanos(self.health_timeout),
                    start_period_ns: nanos(self.health_start_period),
                    retries: self.health_retries.unwrap_or_default(),
                }))
            }
        }
    }

    fn cgroup_limits(&self) -> Result<CgroupLimits> {
        let swap = match self.memory_swap.as_deref() {
            None => 0,
            Some(text) if text.trim() == "-1" => UNLIMITED,
            Some(text) => i64::try_from(parse_size_bytes(text)?)?,
        };
        Ok(CgroupLimits {
            memory_swap_bytes: swap,
            memory_reservation_bytes: self
                .memory_reservation
                .as_deref()
                .map(parse_size_bytes)
                .transpose()?
                .unwrap_or_default(),
            cpu_shares: self.cpu_shares.unwrap_or_default(),
            cpuset_cpus: self.cpuset_cpus.clone().unwrap_or_default(),
            cpu_period: self.cpu_period.unwrap_or_default(),
            cpu_quota: self.cpu_quota.unwrap_or_default(),
        })
    }
}

/// Parse `NAME=SOFT[:HARD]` of `--ulimit`. HARD defaults to SOFT.
fn parse_ulimit(spec: &str) -> Result<Ulimit> {
    let bad = || anyhow::anyhow!("invalid --ulimit '{spec}': use NAME=SOFT[:HARD]");
    let (name, values) = spec.split_once(ULIMIT_SEPARATOR).ok_or_else(bad)?;
    let (soft, hard) = values
        .split_once(ULIMIT_HARD_SEPARATOR)
        .unwrap_or((values, values));
    Ok(Ulimit {
        name: name.to_string(),
        soft: soft.parse().map_err(|_| bad())?,
        hard: hard.parse().map_err(|_| bad())?,
    })
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
                volume: String::new(),
                anonymous: false,
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
        TYPE_VOLUME => {
            if size.is_some() || mode.is_some() {
                bail!("--mount {spec}: tmpfs options need type=tmpfs");
            }
            Ok(MountArg::Bind(match source {
                Some(name) => volume::named_mount(name, target, readonly),
                None => volume::anonymous_mount(target, readonly),
            }))
        }
        other => bail!("--mount {spec}: unsupported type '{other}', use bind, volume or tmpfs"),
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
                readonly: true,
                volume: String::new(),
                anonymous: false,
            })
        );
    }

    #[rstest::rstest]
    #[case::explicit("type=volume,dst=/data")]
    #[case::default_type("dst=/data")]
    fn given_volume_mount_without_source_when_parsed_then_it_is_anonymous(#[case] spec: &str) {
        // Act
        let MountArg::Bind(mount) = parse_mount(spec).unwrap() else {
            panic!("expected a volume mount");
        };

        // Assert
        assert!(mount.anonymous && mount.source.is_empty());
        assert_eq!(mount.volume.len(), 64);
    }

    #[test]
    fn given_volume_mount_when_parsed_then_it_names_the_volume() {
        // Act
        let parsed = parse_mount("type=volume,src=data,dst=/data,ro").unwrap();

        // Assert
        assert_eq!(
            parsed,
            MountArg::Bind(Mount {
                source: String::new(),
                target: "/data".into(),
                readonly: true,
                volume: "data".into(),
                anonymous: false,
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
            add_host: vec!["db:10.1.2.3".into()],
            dns_search: vec!["example.com".into()],
            mount: vec![
                "type=bind,src=/tmp/a,dst=/data".into(),
                "type=tmpfs,dst=/cache".into(),
            ],
            ..Default::default()
        };
        let mut request = CreateSandboxRequest::default();

        // Act
        extras.apply(&mut request).unwrap();

        // Assert
        let guest = request.guest.unwrap();
        assert!(guest.read_only_rootfs);
        assert_eq!(guest.shm_size_bytes, 128 << 20);
        assert_eq!(guest.extra_hosts, ["db:10.1.2.3"]);
        assert_eq!(guest.dns_search, ["example.com"]);
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

    #[rstest::rstest]
    #[case::soft_only("nofile=1024", "nofile", 1024, 1024)]
    #[case::both("nproc=100:200", "nproc", 100, 200)]
    #[case::unlimited("memlock=-1", "memlock", -1, -1)]
    fn given_ulimit_flag_when_parsed_then_hard_defaults_to_soft(
        #[case] spec: &str,
        #[case] name: &str,
        #[case] soft: i64,
        #[case] hard: i64,
    ) {
        // Act
        let parsed = parse_ulimit(spec).unwrap();

        // Assert
        assert_eq!(
            (parsed.name.as_str(), parsed.soft, parsed.hard),
            (name, soft, hard)
        );
    }

    #[rstest::rstest]
    #[case::no_value("nofile")]
    #[case::not_a_number("nofile=many")]
    #[case::bad_hard("nofile=1:x")]
    fn given_a_bad_ulimit_flag_when_parsed_then_it_shows_the_format(#[case] spec: &str) {
        let error = parse_ulimit(spec).unwrap_err();

        assert!(error.to_string().contains("NAME=SOFT[:HARD]"), "{error}");
    }

    #[test]
    fn given_cgroup_flags_when_applied_then_sizes_are_bytes_and_swap_may_be_unlimited() {
        // Arrange
        let extras = GuestExtras {
            memory_swap: Some("-1".into()),
            memory_reservation: Some("64m".into()),
            cpu_shares: Some(512),
            cpu_quota: Some(-1),
            ..Default::default()
        };

        // Act
        let limits = extras.cgroup_limits().unwrap();

        // Assert
        assert_eq!(limits.memory_swap_bytes, -1);
        assert_eq!(limits.memory_reservation_bytes, 64 << 20);
        assert_eq!((limits.cpu_shares, limits.cpu_quota), (512, -1));
    }

    #[test]
    fn given_health_flags_when_applied_then_the_test_is_cmd_shell_with_nanosecond_durations() {
        // Arrange
        let extras = GuestExtras {
            health_cmd: Some("curl -f localhost".into()),
            health_interval: Some(std::time::Duration::from_secs(5)),
            health_retries: Some(2),
            ..Default::default()
        };

        // Act
        let check = extras.healthcheck().unwrap().unwrap();

        // Assert
        assert_eq!(check.test, ["CMD-SHELL", "curl -f localhost"]);
        assert_eq!(
            (check.interval_ns, check.timeout_ns, check.retries),
            (5_000_000_000, 0, 2)
        );
    }

    #[rstest::rstest]
    #[case::none_with_cmd(GuestExtras { health_cmd: Some("x".into()), no_healthcheck: true, ..Default::default() }, "conflicting options")]
    #[case::none_with_tuning(GuestExtras { health_retries: Some(1), no_healthcheck: true, ..Default::default() }, "conflicting options")]
    #[case::tuning_without_cmd(GuestExtras { health_retries: Some(1), ..Default::default() }, "need --health-cmd")]
    fn given_conflicting_health_flags_when_applied_then_the_error_says_why(
        #[case] extras: GuestExtras,
        #[case] reason: &str,
    ) {
        let error = extras.healthcheck().unwrap_err();

        assert!(error.to_string().contains(reason), "{error}");
    }

    #[test]
    fn given_no_healthcheck_when_applied_then_the_test_is_none_and_no_flags_means_no_check() {
        let none = GuestExtras {
            no_healthcheck: true,
            ..Default::default()
        };

        assert_eq!(none.healthcheck().unwrap().unwrap().test, ["NONE"]);
        assert!(GuestExtras::default().healthcheck().unwrap().is_none());
    }

    #[test]
    fn given_dns_when_applied_then_it_is_refused_with_the_reason() {
        // Arrange
        let extras = GuestExtras {
            dns: vec!["1.1.1.1".into()],
            ..Default::default()
        };

        // Act
        let error = extras
            .apply(&mut CreateSandboxRequest::default())
            .unwrap_err();

        // Assert
        assert!(error.to_string().contains("--egress"), "{error}");
    }
}
