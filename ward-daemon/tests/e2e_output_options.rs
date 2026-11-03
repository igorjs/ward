// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the output options of `logs` and `events`. They run the
//! real `ward` binary against a stub-backend `wardd`.

mod common;

use std::process::Output;
use std::time::{Duration, Instant};

use common::Daemon;

const POLL_DEADLINE: Duration = Duration::from_secs(10);
const POLL_STEP: Duration = Duration::from_millis(100);
const STUB_OUTPUT: &str = "stub: echo";
const PAST: &str = "2000-01-01T00:00:00Z";
const FUTURE: &str = "2999-01-01T00:00:00Z";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout(&out)
}

/// Waits until the stub has printed, since it prints shortly after the start.
fn wait_for_output(d: &Daemon, name: &str) {
    let deadline = Instant::now() + POLL_DEADLINE;
    while !ok(d, &["logs", name]).contains(STUB_OUTPUT) {
        assert!(Instant::now() < deadline, "no output from {name}");
        std::thread::sleep(POLL_STEP);
    }
}

#[test]
fn given_logs_until_and_details_when_run_then_old_output_shows_and_new_is_hidden() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["run", "-d", "--name", "web1", "alpine", "echo"]);
    wait_for_output(&d, "web1");

    // Act
    let hidden = ok(&d, &["logs", "--until", PAST, "web1"]);
    let shown = ok(&d, &["logs", "--until", FUTURE, "--details", "web1"]);
    let followed = ok(&d, &["logs", "-f", "--until", FUTURE, "web1"]);
    let bad = run(&d, &["logs", "--until", "soon", "web1"]);

    // Assert
    assert!(!hidden.contains(STUB_OUTPUT), "{hidden}");
    assert!(shown.contains(STUB_OUTPUT), "{shown}");
    assert!(followed.contains(STUB_OUTPUT), "{followed}");
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--until"));
}

#[test]
fn given_a_reason_filter_when_events_replayed_then_a_die_without_that_reason_is_left_out() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["run", "-d", "--name", "web1", "alpine", "echo"]);
    ok(&d, &["stop", "web1"]);
    let replay = |filter: &str| {
        ok(
            &d,
            &[
                "events",
                "--since",
                "1h",
                "--until",
                "0s",
                "--filter",
                "event=die",
                "--filter",
                filter,
                "--format",
                "{{.Action}}:{{.Actor.Attributes.reason}}",
            ],
        )
    };

    // Act
    let any_die = replay("event=die");
    let other = replay("reason=oom");

    // Assert
    assert_eq!(other.trim(), "");
    assert!(any_die.trim().starts_with("die:"), "{any_die}");
}
