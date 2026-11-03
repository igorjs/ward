// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `--ulimit`, `--group-add` and Docker's cgroup options: validation, and
//! the guest agent arguments that apply them.

use crate::pb;
use crate::protocol::{ApiError, CgroupLimits, Ulimit};

/// The limits `--ulimit` names, as `docker run --ulimit` lists them.
pub const ULIMIT_NAMES: [&str; 15] = [
    "core",
    "cpu",
    "data",
    "fsize",
    "locks",
    "memlock",
    "msgqueue",
    "nice",
    "nofile",
    "nproc",
    "rss",
    "rtprio",
    "rttime",
    "sigpending",
    "stack",
];
/// The value that means no limit.
pub const UNLIMITED: i64 = -1;
const MAX_ULIMITS: usize = ULIMIT_NAMES.len();
const MAX_GROUPS: usize = 64;
const MAX_GROUP_NAME_LEN: usize = 32;
/// Docker's bounds for `--cpu-shares`; 0 leaves the default.
const MIN_CPU_SHARES: u32 = 2;
const MAX_CPU_SHARES: u32 = 262_144;
/// Bounds of the cgroup v2 `cpu.weight` that shares map onto.
const MIN_CPU_WEIGHT: u64 = 1;
const MAX_CPU_WEIGHT: u64 = 10_000;
/// Bounds of `--cpu-period` and the smallest `--cpu-quota`, in microseconds.
const MIN_CPU_PERIOD: u64 = 1_000;
const MAX_CPU_PERIOD: u64 = 1_000_000;
const MIN_CPU_QUOTA: i64 = 1_000;
/// The period the kernel uses when only a quota is set.
const DEFAULT_CPU_PERIOD: u64 = 100_000;
const BYTES_PER_MIB: u64 = 1024 * 1024;
/// Quota text of `cpu.max` for no limit.
pub const CPU_MAX_UNLIMITED: &str = "max";

/// Agent argument that sets a limit: `--ulimit <name> <soft> <hard>`.
pub const ULIMIT_ARG: &str = "--ulimit";
/// Agent argument that adds a group: `--group-add <name|gid>`.
pub const GROUP_ADD_ARG: &str = "--group-add";
/// Agent argument for `memory.low`: `--memory-low <bytes>`.
pub const MEMORY_LOW_ARG: &str = "--memory-low";
/// Agent argument for `cpu.weight`: `--cpu-weight <weight>`.
pub const CPU_WEIGHT_ARG: &str = "--cpu-weight";
/// Agent argument for `cpu.max`: `--cpu-max <quota|max> <period>`.
pub const CPU_MAX_ARG: &str = "--cpu-max";
/// Agent argument for `cpuset.cpus`: `--cpuset-cpus <list>`.
pub const CPUSET_ARG: &str = "--cpuset-cpus";

fn invalid(message: String) -> ApiError {
    ApiError::InvalidRequest(message)
}

/// Check the limits and return them with a repeated name replaced by its
/// last value, as Docker does.
///
/// # Errors
///
/// Fails on an unknown name, a soft limit above the hard one, or a value
/// below -1.
pub fn ulimits(entries: &[pb::Ulimit]) -> Result<Vec<Ulimit>, ApiError> {
    let mut kept: Vec<Ulimit> = Vec::new();
    for entry in entries {
        if !ULIMIT_NAMES.contains(&entry.name.as_str()) {
            return Err(invalid(format!("invalid ulimit type: {}", entry.name)));
        }
        let (soft, hard) = (entry.soft, entry.hard);
        let below = soft < UNLIMITED || hard < UNLIMITED;
        let soft_above_hard = hard != UNLIMITED && (soft == UNLIMITED || soft > hard);
        if below || soft_above_hard {
            return Err(invalid(format!(
                "ulimit {}: the soft limit must not exceed the hard limit, and -1 means unlimited",
                entry.name
            )));
        }
        kept.retain(|u| u.name != entry.name);
        kept.push(Ulimit {
            name: entry.name.clone(),
            soft,
            hard,
        });
    }
    if kept.len() > MAX_ULIMITS {
        return Err(invalid("too many --ulimit entries".to_string()));
    }
    Ok(kept)
}

/// Check `--group-add` values: a group id or a group name.
///
/// # Errors
///
/// Fails on a value that is neither, or too many.
pub fn group_add(groups: &[String]) -> Result<(), ApiError> {
    if groups.len() > MAX_GROUPS {
        return Err(invalid(format!(
            "at most {MAX_GROUPS} --group-add entries are allowed"
        )));
    }
    for group in groups {
        let numeric = group.parse::<u32>().is_ok();
        let named = !group.is_empty()
            && group.len() <= MAX_GROUP_NAME_LEN
            && group
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !numeric && !named {
            return Err(invalid(format!("invalid --group-add '{group}'")));
        }
    }
    Ok(())
}

/// Check the cgroup options against the sandbox's memory limit.
///
/// # Errors
///
/// Fails on a value Docker or the guest cgroup does not take, with the
/// reason.
pub fn cgroup(limits: &CgroupLimits, memory_mb: u32) -> Result<(), ApiError> {
    let memory = u64::from(memory_mb) * BYTES_PER_MIB;
    if limits.memory_swap_bytes != 0
        && limits.memory_swap_bytes != i64::try_from(memory).unwrap_or(0)
    {
        return Err(invalid(
            "--memory-swap: the guest has no swap, so only a value equal to --memory (no swap) is accepted"
                .to_string(),
        ));
    }
    if limits.memory_reservation_bytes != 0
        && memory != 0
        && limits.memory_reservation_bytes > memory
    {
        return Err(invalid(format!(
            "--memory-reservation ({}) must not exceed --memory ({memory})",
            limits.memory_reservation_bytes
        )));
    }
    let shares = limits.cpu_shares;
    if shares != 0 && !(MIN_CPU_SHARES..=MAX_CPU_SHARES).contains(&shares) {
        return Err(invalid(format!(
            "--cpu-shares must be 0 or from {MIN_CPU_SHARES} to {MAX_CPU_SHARES}"
        )));
    }
    let period = limits.cpu_period;
    if period != 0 && !(MIN_CPU_PERIOD..=MAX_CPU_PERIOD).contains(&period) {
        return Err(invalid(format!(
            "--cpu-period must be 0 or from {MIN_CPU_PERIOD} to {MAX_CPU_PERIOD} microseconds"
        )));
    }
    let quota = limits.cpu_quota;
    if (quota > 0 && quota < MIN_CPU_QUOTA) || quota < UNLIMITED {
        return Err(invalid(format!(
            "--cpu-quota must be -1, 0 or at least {MIN_CPU_QUOTA} microseconds"
        )));
    }
    cpuset(&limits.cpuset_cpus)
}

/// A cpu list such as `0-1,3`.
fn cpuset(list: &str) -> Result<(), ApiError> {
    let range_ok = |part: &str| match part.split_once('-') {
        Some((low, high)) => matches!(
            (low.parse::<u32>(), high.parse::<u32>()),
            (Ok(low), Ok(high)) if low <= high
        ),
        None => part.parse::<u32>().is_ok(),
    };
    if list.is_empty() || list.split(',').all(range_ok) {
        Ok(())
    } else {
        Err(invalid(format!(
            "invalid --cpuset-cpus '{list}': use a list such as 0-1,3"
        )))
    }
}

/// The cgroup v2 `cpu.weight` for Docker's `--cpu-shares`, on runc's scale.
#[must_use]
pub fn cpu_weight(shares: u32) -> u64 {
    let span = u64::from(MAX_CPU_SHARES - MIN_CPU_SHARES);
    let scaled = u64::from(shares - MIN_CPU_SHARES) * (MAX_CPU_WEIGHT - MIN_CPU_WEIGHT) / span;
    MIN_CPU_WEIGHT + scaled
}

/// The agent arguments that apply the limits at boot.
#[must_use]
pub fn agent_args(ulimits: &[Ulimit], groups: &[String], limits: &CgroupLimits) -> Vec<String> {
    let mut args = Vec::new();
    for u in ulimits {
        args.extend([
            ULIMIT_ARG.to_string(),
            u.name.clone(),
            u.soft.to_string(),
            u.hard.to_string(),
        ]);
    }
    for group in groups {
        args.extend([GROUP_ADD_ARG.to_string(), group.clone()]);
    }
    if limits.memory_reservation_bytes != 0 {
        args.extend([
            MEMORY_LOW_ARG.to_string(),
            limits.memory_reservation_bytes.to_string(),
        ]);
    }
    if limits.cpu_shares != 0 {
        args.extend([
            CPU_WEIGHT_ARG.to_string(),
            cpu_weight(limits.cpu_shares).to_string(),
        ]);
    }
    if limits.cpu_quota != 0 || limits.cpu_period != 0 {
        let quota = if limits.cpu_quota > 0 {
            limits.cpu_quota.to_string()
        } else {
            CPU_MAX_UNLIMITED.to_string()
        };
        let period = if limits.cpu_period == 0 {
            DEFAULT_CPU_PERIOD
        } else {
            limits.cpu_period
        };
        args.extend([CPU_MAX_ARG.to_string(), quota, period.to_string()]);
    }
    if !limits.cpuset_cpus.is_empty() {
        args.extend([CPUSET_ARG.to_string(), limits.cpuset_cpus.clone()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMORY_MB: u32 = 64;
    const MEMORY_BYTES: i64 = 64 * 1024 * 1024;

    fn limit(name: &str, soft: i64, hard: i64) -> pb::Ulimit {
        pb::Ulimit {
            name: name.into(),
            soft,
            hard,
        }
    }

    #[test]
    fn ulimits_keep_the_last_value_of_a_name() {
        // Act
        let kept = ulimits(&[
            limit("nofile", 10, 20),
            limit("core", 0, 0),
            limit("nofile", 30, 40),
        ]);

        // Assert
        let kept = kept.unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].soft, 30);
    }

    #[rstest::rstest]
    #[case::unknown("files", 1, 1)]
    #[case::soft_above_hard("nofile", 10, 5)]
    #[case::below_unlimited("nofile", -2, -2)]
    #[case::unlimited_soft_under_hard("nofile", -1, 5)]
    fn ulimits_refuse_bad_entries(#[case] name: &str, #[case] soft: i64, #[case] hard: i64) {
        assert!(ulimits(&[limit(name, soft, hard)]).is_err());
    }

    #[test]
    fn ulimits_accept_unlimited() {
        assert!(ulimits(&[limit("memlock", -1, -1), limit("nofile", 5, -1)]).is_ok());
    }

    #[rstest::rstest]
    #[case::gid("100", true)]
    #[case::name("audio", true)]
    #[case::dotted("dev.team-1", true)]
    #[case::empty("", false)]
    #[case::space("my group", false)]
    #[case::colon("a:b", false)]
    fn group_add_takes_ids_and_names(#[case] group: &str, #[case] ok: bool) {
        assert_eq!(group_add(&[group.to_string()]).is_ok(), ok);
    }

    #[rstest::rstest]
    #[case::none(CgroupLimits::default(), true)]
    #[case::swap_equals_memory(CgroupLimits { memory_swap_bytes: MEMORY_BYTES, ..Default::default() }, true)]
    #[case::swap_unlimited(CgroupLimits { memory_swap_bytes: -1, ..Default::default() }, false)]
    #[case::swap_more(CgroupLimits { memory_swap_bytes: MEMORY_BYTES * 2, ..Default::default() }, false)]
    #[case::reservation_over_memory(CgroupLimits { memory_reservation_bytes: 128 << 20, ..Default::default() }, false)]
    #[case::shares_low(CgroupLimits { cpu_shares: 1, ..Default::default() }, false)]
    #[case::shares_ok(CgroupLimits { cpu_shares: 512, ..Default::default() }, true)]
    #[case::period_low(CgroupLimits { cpu_period: 10, ..Default::default() }, false)]
    #[case::quota_low(CgroupLimits { cpu_quota: 10, ..Default::default() }, false)]
    #[case::quota_unlimited(CgroupLimits { cpu_quota: -1, ..Default::default() }, true)]
    #[case::cpuset(CgroupLimits { cpuset_cpus: "0-1,3".into(), ..Default::default() }, true)]
    #[case::cpuset_reversed(CgroupLimits { cpuset_cpus: "3-1".into(), ..Default::default() }, false)]
    #[case::cpuset_junk(CgroupLimits { cpuset_cpus: "a".into(), ..Default::default() }, false)]
    fn cgroup_options_follow_docker_bounds_and_the_guest_cgroup(
        #[case] limits: CgroupLimits,
        #[case] ok: bool,
    ) {
        assert_eq!(cgroup(&limits, MEMORY_MB).is_ok(), ok);
    }

    #[test]
    fn swap_needs_a_memory_limit_to_equal() {
        let limits = CgroupLimits {
            memory_swap_bytes: MEMORY_BYTES,
            ..Default::default()
        };

        assert!(cgroup(&limits, 0).is_err());
    }

    #[rstest::rstest]
    #[case(2, 1)]
    #[case(1024, 39)]
    #[case(262_144, 10_000)]
    fn shares_map_onto_cpu_weight_like_runc(#[case] shares: u32, #[case] weight: u64) {
        assert_eq!(cpu_weight(shares), weight);
    }

    #[test]
    fn agent_args_list_each_limit_once() {
        // Arrange
        let limits = CgroupLimits {
            memory_reservation_bytes: 1000,
            cpu_shares: 262_144,
            cpu_quota: 50_000,
            cpu_period: 100_000,
            cpuset_cpus: "0".into(),
            ..Default::default()
        };

        // Act
        let args = agent_args(
            &[Ulimit {
                name: "nofile".into(),
                soft: 5,
                hard: 10,
            }],
            &["audio".to_string()],
            &limits,
        );

        // Assert
        assert_eq!(
            args,
            [
                "--ulimit",
                "nofile",
                "5",
                "10",
                "--group-add",
                "audio",
                "--memory-low",
                "1000",
                "--cpu-weight",
                "10000",
                "--cpu-max",
                "50000",
                "100000",
                "--cpuset-cpus",
                "0"
            ]
        );
    }

    #[test]
    fn period_alone_sets_an_unlimited_quota() {
        let limits = CgroupLimits {
            cpu_period: 50_000,
            ..Default::default()
        };

        assert_eq!(agent_args(&[], &[], &limits), ["--cpu-max", "max", "50000"]);
    }
}
