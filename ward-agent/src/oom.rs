// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Counts the processes the guest kernel's OOM killer has ended, so the
//! daemon can tell an out-of-memory kill from any other SIGKILL.
//!
//! The sandbox cgroup's `memory.events` counts kills of its own processes.
//! Without the memory controller the guest-wide `/proc/vmstat` counter
//! stands in; the sandbox is the only workload in the VM.

use std::path::{Path, PathBuf};

use crate::cgroup::SandboxCgroup;

const MEMORY_EVENTS_FILE: &str = "memory.events";
const VMSTAT_PATH: &str = "/proc/vmstat";
const OOM_KILL_KEY: &str = "oom_kill";

/// Both OOM kill counters. The cgroup one only counts kills of its own
/// processes and can stay at zero for a global OOM kill, so the guest-wide
/// one is always read too.
#[derive(Debug, PartialEq, Eq)]
pub struct OomCounter {
    cgroup_events: Option<PathBuf>,
    vmstat: PathBuf,
}

/// Totals read from [`OomCounter`] at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OomReading {
    cgroup: Option<u64>,
    vmstat: Option<u64>,
}

impl OomCounter {
    /// Reads the cgroup's `memory.events` when it has the memory controller,
    /// and `/proc/vmstat` always.
    pub fn for_cgroup(cgroup: Option<&SandboxCgroup>) -> Self {
        Self {
            cgroup_events: cgroup
                .and_then(SandboxCgroup::memory_dir)
                .map(|dir| dir.join(MEMORY_EVENTS_FILE)),
            vmstat: PathBuf::from(VMSTAT_PATH),
        }
    }

    /// The totals so far. A counter the kernel does not expose reads `None`.
    pub fn read(&self) -> OomReading {
        OomReading {
            cgroup: self.cgroup_events.as_deref().and_then(read_counter),
            vmstat: read_counter(&self.vmstat),
        }
    }
}

/// Kills between two readings: the larger of what either counter saw. Zero
/// when a counter is missing from either reading.
pub fn kills_since(before: OomReading, after: OomReading) -> u64 {
    let delta = |before: Option<u64>, after: Option<u64>| match (before, after) {
        (Some(before), Some(after)) => after.saturating_sub(before),
        _ => 0,
    };
    delta(before.cgroup, after.cgroup).max(delta(before.vmstat, after.vmstat))
}

fn read_counter(path: &Path) -> Option<u64> {
    parse_counter(&std::fs::read_to_string(path).ok()?)
}

/// Both files list `key value` lines; pick the OOM kill count.
fn parse_counter(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == OOM_KILL_KEY).then(|| value.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENTS: &str = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n";

    #[test]
    fn given_memory_events_when_parsed_then_oom_kill_is_read_not_oom() {
        assert_eq!(parse_counter(EVENTS), Some(2));
    }

    #[test]
    fn given_vmstat_when_parsed_then_oom_kill_is_read() {
        assert_eq!(parse_counter("nr_free_pages 5\noom_kill 7\n"), Some(7));
    }

    #[test]
    fn given_no_oom_kill_line_when_parsed_then_none() {
        assert_eq!(parse_counter("low 0\nmax 1\n"), None);
    }

    #[test]
    fn given_garbled_value_when_parsed_then_none() {
        assert_eq!(parse_counter("oom_kill many\n"), None);
    }

    fn fake_cgroup(controllers: &str, events: Option<&str>) -> (tempfile::TempDir, SandboxCgroup) {
        let root = tempfile::tempdir().unwrap();
        let group = crate::cgroup::group_dir(root.path());
        std::fs::create_dir(&group).unwrap();
        std::fs::write(root.path().join("cgroup.controllers"), controllers).unwrap();
        std::fs::write(root.path().join("cgroup.subtree_control"), "").unwrap();
        std::fs::write(group.join("cgroup.procs"), "").unwrap();
        std::fs::write(group.join("pids.max"), "").unwrap();
        if let Some(events) = events {
            std::fs::write(group.join(MEMORY_EVENTS_FILE), events).unwrap();
        }
        let cgroup = crate::cgroup::setup(root.path(), Some(20)).unwrap();
        (root, cgroup)
    }

    #[test]
    fn given_cgroup_with_memory_when_counter_read_then_its_events_file_is_used() {
        let (_root, cgroup) = fake_cgroup("memory pids", Some(EVENTS));

        let reading = OomCounter::for_cgroup(Some(&cgroup)).read();

        assert_eq!(reading.cgroup, Some(2));
    }

    #[test]
    fn given_cgroup_without_memory_when_counter_read_then_only_vmstat_is_used() {
        let (_root, cgroup) = fake_cgroup("pids", None);

        let counter = OomCounter::for_cgroup(Some(&cgroup));

        assert_eq!(counter.cgroup_events, None);
        assert_eq!(counter.vmstat, Path::new(VMSTAT_PATH));
    }

    #[test]
    fn given_no_cgroup_when_counter_built_then_only_vmstat_is_used() {
        let counter = OomCounter::for_cgroup(None);

        assert_eq!(counter.cgroup_events, None);
    }

    #[test]
    fn given_missing_files_when_read_then_none() {
        let counter = OomCounter {
            cgroup_events: Some(PathBuf::from("/nonexistent/ward-test/memory.events")),
            vmstat: PathBuf::from("/nonexistent/ward-test/vmstat"),
        };

        assert_eq!(counter.read(), reading(None, None));
    }

    fn reading(cgroup: Option<u64>, vmstat: Option<u64>) -> OomReading {
        OomReading { cgroup, vmstat }
    }

    #[rstest::rstest]
    #[case::cgroup_saw_it(reading(Some(1), Some(5)), reading(Some(4), Some(5)), 3)]
    #[case::only_global_counter_saw_it(reading(Some(0), Some(5)), reading(Some(0), Some(6)), 1)]
    #[case::larger_delta_wins(reading(Some(0), Some(0)), reading(Some(2), Some(1)), 2)]
    #[case::counter_went_backwards(reading(Some(4), Some(4)), reading(Some(1), Some(1)), 0)]
    #[case::missing_cgroup_counter(reading(None, Some(1)), reading(Some(9), Some(3)), 2)]
    #[case::nothing_readable(reading(None, None), reading(None, None), 0)]
    fn given_two_readings_when_diffed_then_the_larger_counter_delta_counts(
        #[case] before: OomReading,
        #[case] after: OomReading,
        #[case] want: u64,
    ) {
        assert_eq!(kills_since(before, after), want);
    }
}
