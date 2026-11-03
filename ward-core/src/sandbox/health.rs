// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Health checks, following Docker's monitor: the state a run of checks
//! leads to, and the check a request asks for.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use crate::protocol::{ApiError, HealthLogEntry, HealthState, Healthcheck};

pub const STATUS_STARTING: &str = "starting";
pub const STATUS_HEALTHY: &str = "healthy";
pub const STATUS_UNHEALTHY: &str = "unhealthy";

/// Docker's defaults for a health check.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_RETRIES: u32 = 3;
/// Checks Docker keeps in the log.
pub const MAX_LOG_ENTRIES: usize = 5;
/// Bytes of output Docker keeps for a check.
pub const MAX_OUTPUT_BYTES: usize = 4096;
/// Docker's smallest interval, timeout and start period.
const MIN_DURATION: Duration = Duration::from_millis(1);
/// Longest command a request may carry.
const MAX_COMMAND_LEN: usize = 4096;
/// Exit code Docker records for a check that ran out of time or never ran.
pub const FAILED_EXIT_CODE: i32 = -1;

pub const TEST_SHELL: &str = "CMD-SHELL";
pub const TEST_EXEC: &str = "CMD";
pub const TEST_NONE: &str = "NONE";
/// Shell that runs a `CMD-SHELL` check, as Docker uses.
pub const SHELL_PATH: &str = "/bin/sh";
pub const SHELL_FLAG: &str = "-c";

/// A health check with its defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// The argv the guest runs.
    pub argv: Vec<String>,
    pub interval: Duration,
    pub timeout: Duration,
    pub start_period: Duration,
    pub retries: u32,
}

impl Check {
    /// The check to run, or `None` when the sandbox has none (`NONE` or no
    /// test).
    #[must_use]
    pub fn from_config(config: &Healthcheck) -> Option<Self> {
        let argv = match config.test.split_first() {
            Some((kind, rest)) if kind == TEST_SHELL => vec![
                SHELL_PATH.to_string(),
                SHELL_FLAG.to_string(),
                rest.join(" "),
            ],
            Some((kind, rest)) if kind == TEST_EXEC => rest.to_vec(),
            _ => return None,
        };
        let or_default = |ns: u64, default: Duration| {
            if ns == 0 {
                default
            } else {
                Duration::from_nanos(ns)
            }
        };
        Some(Self {
            argv,
            interval: or_default(config.interval_ns, DEFAULT_INTERVAL),
            timeout: or_default(config.timeout_ns, DEFAULT_TIMEOUT),
            start_period: Duration::from_nanos(config.start_period_ns),
            retries: if config.retries == 0 {
                DEFAULT_RETRIES
            } else {
                config.retries
            },
        })
    }
}

/// Check a request's health check.
///
/// # Errors
///
/// Fails on an unknown test form, an empty or oversized command, or a
/// duration under a millisecond.
pub fn validate(config: &Healthcheck) -> Result<(), ApiError> {
    let invalid = |why: &str| Err(ApiError::InvalidRequest(format!("healthcheck: {why}")));
    match config.test.split_first() {
        None => return invalid("test is empty"),
        Some((kind, rest)) if kind == TEST_NONE => {
            return if rest.is_empty() {
                Ok(())
            } else {
                invalid("NONE takes no command")
            };
        }
        Some((kind, rest)) if kind == TEST_SHELL || kind == TEST_EXEC => {
            let command = rest.join(" ");
            if command.trim().is_empty() || command.len() > MAX_COMMAND_LEN {
                return invalid("the command is empty or too long");
            }
        }
        Some(_) => return invalid("test must start with CMD, CMD-SHELL or NONE"),
    }
    let too_short = |ns: u64| ns != 0 && Duration::from_nanos(ns) < MIN_DURATION;
    if too_short(config.interval_ns)
        || too_short(config.timeout_ns)
        || too_short(config.start_period_ns)
    {
        return invalid("interval, timeout and start period must be at least 1ms");
    }
    Ok(())
}

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub start: SystemTime,
    pub end: SystemTime,
    pub exit_code: i32,
    pub output: String,
}

/// Cut `output` to Docker's limit on a character boundary.
#[must_use]
pub fn truncate_output(output: &str) -> String {
    cut_at(output, MAX_OUTPUT_BYTES).to_string()
}

/// Add `chunk` to `output` without letting it pass Docker's limit.
pub fn append_capped(output: &mut String, chunk: &str) {
    let room = MAX_OUTPUT_BYTES.saturating_sub(output.len());
    output.push_str(cut_at(chunk, room));
}

fn cut_at(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The checks of one run of a sandbox.
#[derive(Debug, Clone)]
pub struct Tracker {
    /// When the run started, which tells a new run from the one tracked.
    pub run_started: Option<SystemTime>,
    started_at: Instant,
    status: &'static str,
    failing_streak: u32,
    log: VecDeque<HealthLogEntry>,
    next_due: Instant,
    in_flight: bool,
}

impl Tracker {
    /// Begin tracking a run that started at `started_at`; the first check is
    /// one interval after it.
    #[must_use]
    pub fn new(run_started: Option<SystemTime>, started_at: Instant, interval: Duration) -> Self {
        Self {
            run_started,
            started_at,
            status: STATUS_STARTING,
            failing_streak: 0,
            log: VecDeque::new(),
            next_due: started_at + interval,
            in_flight: false,
        }
    }

    /// Whether a check should start now. Marks it started.
    pub fn take_due(&mut self, now: Instant) -> bool {
        if self.in_flight || now < self.next_due {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// Give up the check that was due, to try again after the interval.
    pub fn skip(&mut self, check: &Check, now: Instant) {
        self.in_flight = false;
        self.next_due = now + check.interval;
    }

    /// Add a finished check. Returns the new status when it changed.
    ///
    /// A failure within the start period does not count, but a success
    /// ends it, as in Docker. The period is measured to when the check
    /// began, not when it ended.
    pub fn record(
        &mut self,
        probe: Probe,
        check: &Check,
        began: Instant,
        now: Instant,
    ) -> Option<&'static str> {
        self.in_flight = false;
        self.next_due = now + check.interval;
        let before = self.status;
        if probe.exit_code == 0 {
            self.status = STATUS_HEALTHY;
            self.failing_streak = 0;
        } else if began.saturating_duration_since(self.started_at) >= check.start_period {
            self.failing_streak += 1;
            if self.failing_streak >= check.retries {
                self.status = STATUS_UNHEALTHY;
            }
        }
        self.log.push_back(HealthLogEntry {
            start: probe.start,
            end: probe.end,
            exit_code: probe.exit_code,
            output: probe.output,
        });
        while self.log.len() > MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        (self.status != before).then_some(self.status)
    }

    /// The state of a run that has had no check yet.
    #[must_use]
    pub fn starting_state() -> HealthState {
        HealthState {
            status: STATUS_STARTING.to_string(),
            failing_streak: 0,
            log: Vec::new(),
        }
    }

    /// The state as `inspect` reports it.
    #[must_use]
    pub fn state(&self) -> HealthState {
        HealthState {
            status: self.status.to_string(),
            failing_streak: self.failing_streak,
            log: self.log.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_secs(10);

    fn check(retries: u32, start_period: Duration) -> Check {
        Check {
            argv: vec!["true".into()],
            interval: INTERVAL,
            timeout: DEFAULT_TIMEOUT,
            start_period,
            retries,
        }
    }

    fn probe(exit_code: i32) -> Probe {
        Probe {
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH,
            exit_code,
            output: format!("exit {exit_code}"),
        }
    }

    fn config(test: &[&str]) -> Healthcheck {
        Healthcheck {
            test: test.iter().map(ToString::to_string).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_shell_check_runs_under_bin_sh_with_docker_defaults() {
        // Act
        let check = Check::from_config(&config(&["CMD-SHELL", "curl -f localhost"])).unwrap();

        // Assert
        assert_eq!(check.argv, ["/bin/sh", "-c", "curl -f localhost"]);
        assert_eq!(
            (check.interval, check.timeout, check.retries),
            (DEFAULT_INTERVAL, DEFAULT_TIMEOUT, DEFAULT_RETRIES)
        );
        assert_eq!(check.start_period, Duration::ZERO);
    }

    #[test]
    fn an_exec_check_runs_its_argv_and_none_has_no_check() {
        assert_eq!(
            Check::from_config(&config(&["CMD", "pg_isready", "-q"]))
                .unwrap()
                .argv,
            ["pg_isready", "-q"]
        );
        assert!(Check::from_config(&config(&["NONE"])).is_none());
    }

    #[rstest::rstest]
    #[case::shell(&["CMD-SHELL", "true"], true)]
    #[case::exec(&["CMD", "true"], true)]
    #[case::none(&["NONE"], true)]
    #[case::empty(&[], false)]
    #[case::unknown(&["RUN", "true"], false)]
    #[case::blank_command(&["CMD-SHELL", "  "], false)]
    #[case::none_with_command(&["NONE", "true"], false)]
    fn validate_takes_the_three_test_forms(#[case] test: &[&str], #[case] ok: bool) {
        assert_eq!(validate(&config(test)).is_ok(), ok);
    }

    #[test]
    fn validate_refuses_a_duration_under_a_millisecond() {
        let mut short = config(&["CMD-SHELL", "true"]);
        short.interval_ns = 10;

        assert!(validate(&short).is_err());
    }

    #[test]
    fn retries_failures_make_it_unhealthy_and_a_success_resets_the_streak() {
        // Arrange
        let check = check(2, Duration::ZERO);
        let start = Instant::now();
        let mut tracker = Tracker::new(None, start, INTERVAL);

        // Act
        let first = tracker.record(probe(1), &check, start + INTERVAL, start + INTERVAL);
        let second = tracker.record(probe(1), &check, start + INTERVAL * 2, start + INTERVAL * 2);
        let third = tracker.record(probe(0), &check, start + INTERVAL * 3, start + INTERVAL * 3);

        // Assert
        assert_eq!(first, None);
        assert_eq!(second, Some(STATUS_UNHEALTHY));
        assert_eq!(third, Some(STATUS_HEALTHY));
        assert_eq!(tracker.state().failing_streak, 0);
    }

    #[test]
    fn failures_in_the_start_period_do_not_count_but_a_success_does_end_it() {
        // Arrange
        let check = check(1, Duration::from_secs(60));
        let start = Instant::now();
        let mut tracker = Tracker::new(None, start, INTERVAL);

        // Act
        let early = tracker.record(probe(1), &check, start + INTERVAL, start + INTERVAL);
        let state = tracker.state();
        let healthy = tracker.record(probe(0), &check, start + INTERVAL * 2, start + INTERVAL * 2);
        let late = tracker.record(
            probe(1),
            &check,
            start + Duration::from_secs(120),
            start + Duration::from_secs(120),
        );

        // Assert
        assert_eq!(early, None);
        assert_eq!(
            (state.status.as_str(), state.failing_streak),
            (STATUS_STARTING, 0)
        );
        assert_eq!(healthy, Some(STATUS_HEALTHY));
        assert_eq!(late, Some(STATUS_UNHEALTHY));
    }

    #[test]
    fn the_log_keeps_the_last_five_checks_oldest_first() {
        // Arrange
        let check = check(3, Duration::ZERO);
        let start = Instant::now();
        let mut tracker = Tracker::new(None, start, INTERVAL);

        // Act
        for code in 0..7 {
            tracker.record(probe(code), &check, start + INTERVAL, start + INTERVAL);
        }

        // Assert
        let codes: Vec<i32> = tracker.state().log.iter().map(|e| e.exit_code).collect();
        assert_eq!(codes, [2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_check_is_due_one_interval_after_the_run_starts_and_never_twice_at_once() {
        // Arrange
        let check = check(3, Duration::ZERO);
        let start = Instant::now();
        let mut tracker = Tracker::new(None, start, INTERVAL);

        // Act / Assert
        assert!(!tracker.take_due(start + INTERVAL / 2));
        assert!(tracker.take_due(start + INTERVAL));
        assert!(!tracker.take_due(start + INTERVAL * 2));
        tracker.record(probe(0), &check, start + INTERVAL * 2, start + INTERVAL * 2);
        assert!(!tracker.take_due(start + INTERVAL * 2));
        assert!(tracker.take_due(start + INTERVAL * 3));
    }

    #[test]
    fn output_is_cut_at_docker_limit_on_a_character_boundary() {
        // Arrange
        let long = "é".repeat(MAX_OUTPUT_BYTES);

        // Act
        let cut = truncate_output(&long);

        // Assert
        assert!(cut.len() <= MAX_OUTPUT_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(truncate_output("ok"), "ok");
    }

    #[test]
    fn the_start_period_is_measured_to_when_the_check_began() {
        // Arrange
        let check = check(1, Duration::from_secs(60));
        let start = Instant::now();
        let mut tracker = Tracker::new(None, start, INTERVAL);
        let began = start + Duration::from_secs(50);
        let ended = start + Duration::from_secs(70);

        // Act
        let status = tracker.record(probe(1), &check, began, ended);

        // Assert
        assert_eq!(status, None);
    }

    #[test]
    fn appended_output_stops_at_the_limit_and_never_splits_a_character() {
        // Arrange
        let mut output = String::new();

        // Act
        for _ in 0..MAX_OUTPUT_BYTES {
            append_capped(&mut output, "é");
        }
        append_capped(&mut output, "more");

        // Assert
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        assert!(output.len() >= MAX_OUTPUT_BYTES - 1);
    }
}
