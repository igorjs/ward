// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward publish` and `ward subscribe`.
//!
//! The broker is unimplemented in the daemon today; these scenarios
//! validate the user-facing contract while the broker is built:
//!
//!   1. Bad inputs (`.bad-topic`, empty topic) get an InvalidArgument
//!      error message that mentions the offending field.
//!   2. Valid inputs surface as an "unimplemented" error — the user is
//!      told the feature isn't there yet, not that their input was bad.
//!
//! When the broker lands, this file expands with publish-then-receive
//! scenarios. The negative-path coverage stays in place.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Feature: publish
// ---------------------------------------------------------------------------

#[test]
fn given_running_daemon_when_user_publishes_with_leading_dot_topic_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: leading-dot topics are ambiguous for routing; the validator
    // rejects them before the broker is even reached.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["publish", "deadbeef", ".invalid-topic", "hello"])
        .assert();

    // Assert: non-zero exit and stderr mentions the offending field so
    // a user typo at the shell surfaces with a useful error.
    assertion
        .failure()
        .stderr(predicate::str::contains("topic"));
}

#[test]
fn given_running_daemon_when_user_publishes_with_empty_topic_then_fails() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["publish", "deadbeef", "", "hello"]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("topic"));
}

#[test]
fn given_running_daemon_when_user_publishes_with_valid_args_then_fails_with_unimplemented() {
    // Arrange: this is the "broker stub contract" test at the E2E layer.
    // A well-formed publish must surface as "unimplemented" rather than
    // a validation error, so users know the broker is missing rather
    // than thinking their input is bad.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["publish", "deadbeef", "agent.results.build", "hello"])
        .assert();

    // Assert: non-zero exit, stderr contains the "Unimplemented" status.
    // When the broker lands, this test becomes the regression guard that
    // confirms it ALSO accepts valid inputs (just with a different
    // success criterion).
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
}

// ---------------------------------------------------------------------------
// Feature: subscribe
// ---------------------------------------------------------------------------

#[test]
fn given_running_daemon_when_user_subscribes_with_empty_topic_then_fails() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["subscribe", "deadbeef", ""]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("topic"));
}

#[test]
fn given_running_daemon_when_user_subscribes_with_valid_args_then_fails_with_unimplemented() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["subscribe", "deadbeef", "agent.events"]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
}
