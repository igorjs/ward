// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Freezes and thaws every process of the sandbox cgroup with the cgroup v2
//! freezer. The agent itself runs outside that cgroup, so it keeps
//! answering the daemon while the sandbox is frozen.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

const FREEZE_FILE: &str = "cgroup.freeze";
const EVENTS_FILE: &str = "cgroup.events";
const FROZEN_KEY: &str = "frozen";
const FROZEN_VALUE: &str = "1";
const THAWED_VALUE: &str = "0";
/// The kernel applies a freeze asynchronously; this is how long to wait for
/// it to report the new state.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// Freeze (`true`) or thaw (`false`) the cgroup at `dir`, returning once the
/// kernel reports that state in `cgroup.events`.
///
/// # Errors
///
/// Fails when the guest kernel has no cgroup freezer, or when the cgroup does
/// not reach the state within a few seconds.
pub fn set_frozen(dir: &Path, frozen: bool) -> io::Result<()> {
    let wanted = if frozen { FROZEN_VALUE } else { THAWED_VALUE };
    std::fs::write(dir.join(FREEZE_FILE), wanted)?;
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        if reported_state(dir)? == frozen {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the sandbox did not reach the requested state in time",
            ));
        }
        std::thread::sleep(SETTLE_POLL);
    }
}

fn reported_state(dir: &Path) -> io::Result<bool> {
    let events = std::fs::read_to_string(dir.join(EVENTS_FILE))?;
    Ok(events.lines().any(|line| {
        let mut parts = line.split_whitespace();
        parts.next() == Some(FROZEN_KEY) && parts.next() == Some(FROZEN_VALUE)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cgroup(frozen: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FREEZE_FILE), "").unwrap();
        let state = if frozen { FROZEN_VALUE } else { THAWED_VALUE };
        std::fs::write(
            dir.path().join(EVENTS_FILE),
            format!("populated 1\nfrozen {state}\n"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn given_cgroup_reporting_frozen_when_frozen_then_freeze_file_holds_one() {
        let dir = fake_cgroup(true);

        set_frozen(dir.path(), true).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(FREEZE_FILE)).unwrap(),
            FROZEN_VALUE
        );
    }

    #[test]
    fn given_cgroup_reporting_thawed_when_thawed_then_freeze_file_holds_zero() {
        let dir = fake_cgroup(false);

        set_frozen(dir.path(), false).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(FREEZE_FILE)).unwrap(),
            THAWED_VALUE
        );
    }

    #[test]
    fn given_cgroup_without_freezer_when_frozen_then_it_fails() {
        let dir = tempfile::tempdir().unwrap();

        assert!(set_frozen(dir.path(), true).is_err());
    }
}
