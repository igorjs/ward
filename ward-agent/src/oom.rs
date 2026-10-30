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

/// Where the running total of OOM kills is read from.
#[derive(Debug, PartialEq, Eq)]
pub struct OomCounter {
    path: PathBuf,
}

impl OomCounter {
    /// The cgroup's own counter when it has the memory controller, the
    /// guest-wide one otherwise.
    pub fn for_cgroup(cgroup: Option<&SandboxCgroup>) -> Self {
        let path = match cgroup.and_then(SandboxCgroup::memory_dir) {
            Some(dir) => dir.join(MEMORY_EVENTS_FILE),
            None => PathBuf::from(VMSTAT_PATH),
        };
        Self { path }
    }

    /// The total so far, or `None` when the kernel does not expose one.
    pub fn read(&self) -> Option<u64> {
        read_counter(&self.path)
    }
}

/// Kills between two readings. Zero when either reading is missing.
pub fn kills_since(before: Option<u64>, after: Option<u64>) -> u64 {
    match (before, after) {
        (Some(before), Some(after)) => after.saturating_sub(before),
        _ => 0,
    }
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

    #[test]
    fn given_cgroup_with_memory_when_counter_read_then_its_events_file_is_used() {
        let root = tempfile::tempdir().unwrap();
        let group = crate::cgroup::group_dir(root.path());
        std::fs::create_dir(&group).unwrap();
        for file in ["cgroup.controllers", "cgroup.subtree_control"] {
            std::fs::write(root.path().join(file), "memory pids").unwrap();
        }
        std::fs::write(group.join("cgroup.procs"), "").unwrap();
        std::fs::write(group.join("pids.max"), "").unwrap();
        std::fs::write(group.join(MEMORY_EVENTS_FILE), EVENTS).unwrap();
        let cgroup = crate::cgroup::setup(root.path(), 20).unwrap();

        let counter = OomCounter::for_cgroup(Some(&cgroup));

        assert_eq!(counter.read(), Some(2));
    }

    #[test]
    fn given_cgroup_without_memory_when_counter_built_then_vmstat_is_used() {
        let root = tempfile::tempdir().unwrap();
        let group = crate::cgroup::group_dir(root.path());
        std::fs::create_dir(&group).unwrap();
        std::fs::write(root.path().join("cgroup.controllers"), "pids").unwrap();
        std::fs::write(root.path().join("cgroup.subtree_control"), "").unwrap();
        std::fs::write(group.join("cgroup.procs"), "").unwrap();
        std::fs::write(group.join("pids.max"), "").unwrap();
        let cgroup = crate::cgroup::setup(root.path(), 20).unwrap();

        let counter = OomCounter::for_cgroup(Some(&cgroup));

        assert_eq!(counter.path, Path::new(VMSTAT_PATH));
    }

    #[test]
    fn given_no_cgroup_when_counter_built_then_vmstat_is_used() {
        let counter = OomCounter::for_cgroup(None);

        assert_eq!(counter.path, Path::new(VMSTAT_PATH));
    }

    #[test]
    fn given_missing_file_when_read_then_none() {
        let counter = OomCounter {
            path: PathBuf::from("/nonexistent/ward-test/memory.events"),
        };

        assert_eq!(counter.read(), None);
    }

    #[test]
    fn given_two_readings_when_diffed_then_the_difference_counts() {
        assert_eq!(kills_since(Some(1), Some(4)), 3);
        assert_eq!(kills_since(Some(4), Some(1)), 0);
        assert_eq!(kills_since(None, Some(4)), 0);
        assert_eq!(kills_since(Some(1), None), 0);
    }
}
