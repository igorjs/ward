// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward attach`, `ward start -a` and `ward run -a` with
//! the stub backend, which prints `stub: <argv0>` and exits 0.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::process::Output;
use std::time::{Duration, Instant};

use common::Daemon;

const POLL_STEP: Duration = Duration::from_millis(100);
const POLL_DEADLINE: Duration = Duration::from_secs(30);

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    stdout(&out).trim().to_string()
}

/// A sandbox whose stub command has run and ended.
fn exited_sandbox(d: &Daemon, name: &str) {
    ok(d, &["create", "--name", name, "alpine", "echo", "hi"]);
    let deadline = Instant::now() + POLL_DEADLINE;
    while ok(d, &["inspect", "--format", "{{.State.Status}}", name]) != "exited" {
        assert!(Instant::now() < deadline, "{name} never exited");
        std::thread::sleep(POLL_STEP);
    }
}

#[test]
fn given_exited_sandbox_when_started_with_a_then_its_output_is_printed_and_exit_is_zero() {
    // Arrange
    let d = Daemon::spawn();
    exited_sandbox(&d, "job");

    // Act
    let out = run(&d, &["start", "-a", "job"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("stub: echo"), "{}", stdout(&out));
}

#[test]
fn given_exited_sandbox_when_started_with_i_then_it_attaches_too() {
    // Arrange
    let d = Daemon::spawn();
    exited_sandbox(&d, "job");

    // Act
    let out = d
        .cli()
        .args(["container", "start", "-ai", "job"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ward");

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("stub: echo"));
}

#[test]
fn given_two_sandboxes_when_started_with_a_then_it_refuses() {
    // Arrange
    let d = Daemon::spawn();
    exited_sandbox(&d, "one");
    exited_sandbox(&d, "two");

    // Act
    let out = run(&d, &["start", "-a", "one", "two"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("cannot start and attach multiple"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_exited_sandbox_when_attached_then_it_fails_like_docker() {
    // Arrange
    let d = Daemon::spawn();
    exited_sandbox(&d, "job");

    // Act
    let out = run(&d, &["attach", "job"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("cannot attach to a stopped container"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_a_bad_detach_key_when_attaching_then_it_fails_before_attaching() {
    // Arrange
    let d = Daemon::spawn();
    exited_sandbox(&d, "job");

    // Act
    let out = run(&d, &["start", "-a", "--detach-keys", "nope", "job"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("invalid detach keys"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_run_a_stdout_when_it_runs_then_stdout_is_shown() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["run", "-a", "stdout", "alpine", "echo", "hi"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("stub: echo"));
}

#[test]
fn given_run_a_stderr_only_when_it_runs_then_stdout_is_hidden() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["run", "-a", "STDERR", "alpine", "echo", "hi"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

#[test]
fn given_run_a_with_a_bad_stream_when_parsed_then_it_names_the_valid_ones() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["run", "-a", "stdlog", "alpine", "true"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("STDIN, STDOUT, and STDERR"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_run_a_with_d_when_parsed_then_the_options_conflict() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["run", "-a", "stdout", "-d", "alpine", "true"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("conflicting options: -a and -d"),
        "{}",
        stderr(&out)
    );
}
