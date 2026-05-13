// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward publish` and `ward subscribe`.
//!
//! The broker is wired end-to-end; the gRPC layer routes through
//! `SandboxManager::broker()` which holds per-sandbox CommunicationPolicy.
//!
//! Scope of this file (validation + lifecycle, no two-sandbox fan-out):
//!
//!   1. Bad inputs (`.bad-topic`, empty topic) get a validation error
//!      that mentions the offending field.
//!   2. Valid inputs against an unregistered sandbox surface as
//!      "NotFound" — the broker doesn't know that sandbox.
//!
//! The positive cross-sandbox fan-out E2E lives separately and depends
//! on the `--comms-mode` / `--comms-group` CLI flags landing first.

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
fn given_running_daemon_when_user_publishes_for_unregistered_sandbox_then_not_found() {
    // Arrange: well-formed publish, but the sandbox id is not registered
    // with the broker (no `ward create` ran). The broker returns
    // SandboxNotFound; the CLI surfaces the gRPC NotFound status.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["publish", "deadbeef", "agent.results.build", "hello"])
        .assert();

    // Assert: stderr names the offending sandbox id so users can grep
    // CI logs and immediately spot what's missing.
    assertion
        .failure()
        .stderr(predicate::str::contains("deadbeef"));
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
fn given_running_daemon_when_user_subscribes_for_unregistered_sandbox_then_not_found() {
    // Arrange: symmetric with the publish case — unregistered sandbox
    // surfaces as NotFound on subscribe too.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["subscribe", "deadbeef", "agent.events"]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("deadbeef"));
}
