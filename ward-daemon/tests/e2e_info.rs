// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward info` and `ward health`.
//!
//! These tests simulate a user at a shell: spawn the real `wardd` daemon
//! as a subprocess, run the real `ward` CLI binary, and assert on the
//! actual stdout / stderr / exit code the user would see.
//!
//! Style:
//!   - Function names follow BDD: `given_X_when_Y_then_Z`.
//!   - Bodies use AAA markers (// Arrange / // Act / // Assert).
//!   - One scenario per test; the `Daemon` guard provides per-test
//!     isolation so they can run in parallel.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Feature: daemon info
// ---------------------------------------------------------------------------

#[test]
fn given_running_daemon_when_user_runs_info_then_command_succeeds() {
    // Arrange: a daemon is running on a per-test socket.
    let daemon = common::Daemon::spawn();

    // Act: the user types `ward info`.
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("info").assert();

    // Assert: the command exits 0.
    assertion.success();
}

#[test]
fn given_running_daemon_when_user_runs_info_then_output_includes_version() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("info").assert();

    // Assert: stdout contains a `version: <semver>` line. We don't pin
    // the version string (it changes across releases) but the *prefix*
    // is the public CLI contract — scripts grep for it.
    assertion
        .success()
        .stdout(predicate::str::contains("version:"));
}

#[test]
fn given_running_daemon_when_user_runs_info_then_output_includes_platform_arch_backend() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("info").assert();

    // Assert: every line the user expects is present. If a future
    // refactor accidentally drops one, this test fails loud.
    assertion
        .success()
        .stdout(predicate::str::contains("platform:"))
        .stdout(predicate::str::contains("arch:"))
        .stdout(predicate::str::contains("backend: krunvm"));
}

// ---------------------------------------------------------------------------
// Feature: daemon health
// ---------------------------------------------------------------------------

#[test]
fn given_running_daemon_when_user_runs_health_then_status_is_ok() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("health").assert();

    // Assert: a freshly-spawned daemon reports healthy.
    assertion
        .success()
        .stdout(predicate::str::contains("status: ok"));
}

#[test]
fn given_running_daemon_when_user_runs_health_then_sandbox_count_is_zero() {
    // Arrange: a daemon with no sandboxes started.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("health").assert();

    // Assert: zero sandboxes — sanity check that the daemon's accounting
    // starts at zero and the CLI surfaces it as plain integer text.
    assertion
        .success()
        .stdout(predicate::str::contains("sandbox_count: 0"));
}

#[test]
fn given_running_daemon_when_user_runs_health_then_uptime_field_is_present() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("health").assert();

    // Assert: uptime_seconds appears, value is non-negative (always true
    // for u64 but we still verify the line is rendered).
    assertion
        .success()
        .stdout(predicate::str::contains("uptime_seconds:"));
}

// ---------------------------------------------------------------------------
// Feature: failure paths the user actually sees
// ---------------------------------------------------------------------------

#[test]
fn given_no_running_daemon_when_user_runs_info_then_command_fails_with_clear_error() {
    // Arrange: point the CLI at a socket that doesn't exist. We do *not*
    // spawn a daemon, simulating "user forgot to start the daemon."
    let tmp = tempfile::tempdir().unwrap();
    let bogus_socket = tmp.path().join("nowhere.sock");
    let mut cmd = assert_cmd::Command::cargo_bin("ward").unwrap();
    cmd.env("WARD_SOCKET", &bogus_socket);

    // Act
    let assertion = cmd.arg("info").assert();

    // Assert: non-zero exit and the error mentions the socket path so
    // the user knows where to look. The message text is intentionally
    // not pinned to an exact string — that would be brittle across
    // tonic upgrades. We only verify it mentions the socket file.
    assertion.failure().stderr(predicate::str::contains(
        bogus_socket.file_name().unwrap().to_str().unwrap(),
    ));
}
