// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! When and how fast a sandbox whose main process ended is started again,
//! following Docker's restart manager.

use std::time::{Duration, SystemTime};

use crate::protocol::{RestartKind, RestartPolicy};

/// Wait before the first restart. Each further restart doubles it.
const INITIAL_DELAY: Duration = Duration::from_millis(100);
/// The longest wait between two restarts.
const MAX_DELAY: Duration = Duration::from_secs(60);
/// A run at least this long starts the back-off over.
const STABLE_RUN: Duration = Duration::from_secs(10);

/// A main process that ended and may need starting again.
#[derive(Debug, Clone)]
pub struct RestartRequest {
    pub id: String,
    pub exit_code: i32,
    pub ran_for: Duration,
    /// Identifies the run that ended, so a later run is left alone.
    pub finished_at: Option<SystemTime>,
}

/// Whether `policy` starts the sandbox again after a run that ended with
/// `exit_code`, given the restarts made so far. A manual stop is checked by
/// the caller.
#[must_use]
pub fn wants_restart(policy: RestartPolicy, exit_code: i32, restarts_so_far: u32) -> bool {
    match policy.kind {
        RestartKind::No => false,
        RestartKind::Always | RestartKind::UnlessStopped => true,
        RestartKind::OnFailure => {
            exit_code != 0 && (policy.max_retries == 0 || restarts_so_far < policy.max_retries)
        }
    }
}

/// Whether a sandbox that is not running when the daemon starts is started
/// then. `always` does so even after a manual stop.
#[must_use]
pub fn restarts_at_boot(policy: RestartPolicy, manually_stopped: bool) -> bool {
    match policy.kind {
        RestartKind::Always => true,
        RestartKind::UnlessStopped => !manually_stopped,
        RestartKind::No | RestartKind::OnFailure => false,
    }
}

/// The wait before the next restart: doubled each time, back to the start
/// after a run that lasted.
#[must_use]
pub fn next_delay(previous: Option<Duration>, ran_for: Duration) -> Duration {
    match previous {
        Some(previous) if ran_for < STABLE_RUN => (previous * 2).min(MAX_DELAY),
        _ => INITIAL_DELAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: i32 = 1;
    const OK: i32 = 0;

    fn policy(kind: RestartKind, max_retries: u32) -> RestartPolicy {
        RestartPolicy { kind, max_retries }
    }

    #[rstest::rstest]
    #[case::no(RestartKind::No, 0, FAILED, 0, false)]
    #[case::always_on_success(RestartKind::Always, 0, OK, 5, true)]
    #[case::always_on_failure(RestartKind::Always, 0, FAILED, 5, true)]
    #[case::unless_stopped(RestartKind::UnlessStopped, 0, OK, 0, true)]
    #[case::on_failure_fails(RestartKind::OnFailure, 0, FAILED, 9, true)]
    #[case::on_failure_succeeds(RestartKind::OnFailure, 0, OK, 0, false)]
    #[case::on_failure_under_cap(RestartKind::OnFailure, 3, FAILED, 2, true)]
    #[case::on_failure_at_cap(RestartKind::OnFailure, 3, FAILED, 3, false)]
    fn given_a_policy_when_the_process_ends_then_it_restarts_as_docker_does(
        #[case] kind: RestartKind,
        #[case] max_retries: u32,
        #[case] exit_code: i32,
        #[case] restarts: u32,
        #[case] want: bool,
    ) {
        // Act
        let got = wants_restart(policy(kind, max_retries), exit_code, restarts);

        // Assert
        assert_eq!(got, want);
    }

    #[rstest::rstest]
    #[case::no(RestartKind::No, false, false)]
    #[case::on_failure(RestartKind::OnFailure, false, false)]
    #[case::always(RestartKind::Always, false, true)]
    #[case::always_after_manual_stop(RestartKind::Always, true, true)]
    #[case::unless_stopped(RestartKind::UnlessStopped, false, true)]
    #[case::unless_stopped_after_manual_stop(RestartKind::UnlessStopped, true, false)]
    fn given_a_policy_when_the_daemon_starts_then_only_always_and_unless_stopped_restart(
        #[case] kind: RestartKind,
        #[case] manually_stopped: bool,
        #[case] want: bool,
    ) {
        // Act
        let got = restarts_at_boot(policy(kind, 0), manually_stopped);

        // Assert
        assert_eq!(got, want);
    }

    #[test]
    fn given_quick_failures_when_delays_computed_then_they_double_up_to_a_minute() {
        // Arrange
        let quick = Duration::from_millis(5);
        let mut delay = next_delay(None, quick);
        let mut seen = vec![delay];

        // Act
        for _ in 0..12 {
            delay = next_delay(Some(delay), quick);
            seen.push(delay);
        }

        // Assert
        assert_eq!(seen[0], INITIAL_DELAY);
        assert_eq!(seen[1], INITIAL_DELAY * 2);
        assert_eq!(seen[2], INITIAL_DELAY * 4);
        assert_eq!(*seen.last().unwrap(), MAX_DELAY);
    }

    #[test]
    fn given_a_long_run_when_delay_computed_then_the_back_off_starts_over() {
        // Act
        let delay = next_delay(Some(Duration::from_secs(8)), STABLE_RUN);

        // Assert
        assert_eq!(delay, INITIAL_DELAY);
    }
}
