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

// ---------------------------------------------------------------------------
// Feature: cross-sandbox publish/subscribe (the headline scenario)
// ---------------------------------------------------------------------------

/// Helper: create a sandbox in the named group and return its id.
fn create_in_group(daemon: &common::Daemon, image: &str, group: &str) -> String {
    let out = daemon
        .cli()
        .args([
            "create",
            image,
            "--comms-mode",
            "group",
            "--comms-group",
            group,
        ])
        .output()
        .expect("create");
    assert!(out.status.success(), "create failed: {:?}", out);
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("id: ").map(str::trim))
        .expect("id line")
        .to_string()
}

#[test]
fn given_two_same_group_sandboxes_when_user_publishes_then_subscriber_prints_message() {
    // Arrange: alice + bob both in group "team". Bob subscribes to
    // "events" in a child process; alice publishes from the foreground.
    // The headline cross-agent flow — first time `ward publish` actually
    // delivers a message visible from the CLI side of the wire.
    let daemon = common::Daemon::spawn();
    let alice = create_in_group(&daemon, "alpine:1", "team");
    let bob = create_in_group(&daemon, "alpine:2", "team");

    // Spawn subscriber with stdout piped so we can inspect what arrived
    // after we kill the child.
    let mut sub = daemon
        .cli()
        .args(["subscribe", &bob, "events"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn subscribe");

    // Let the broker register the subscription before we publish. The
    // broker is lossy fan-out, so a publish that lands before the
    // subscriber is registered is dropped silently.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Act
    daemon
        .cli()
        .args(["publish", &alice, "events", "hello-bob"])
        .assert()
        .success()
        .stdout(predicate::str::contains("published"));

    // Let the bridge deliver the message before we kill the subscriber.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Kill the subscriber and read everything it printed.
    sub.kill().expect("kill subscriber");
    let output = sub.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Assert: subscriber stdout contains the payload AND identifies
    // alice as the sender. The CLI prints structured key:value lines.
    assert!(
        stdout.contains("payload: hello-bob"),
        "subscriber stdout missing payload: {stdout}"
    );
    assert!(
        stdout.contains(&format!("from_sandbox: {alice}")),
        "subscriber stdout missing from_sandbox: {stdout}"
    );
}

#[test]
fn given_different_group_sandboxes_when_user_publishes_then_subscriber_sees_nothing() {
    // Arrange: alice in "team-a", bob in "team-b". Group policy blocks
    // cross-group traffic. Bob's subscribe should receive nothing.
    let daemon = common::Daemon::spawn();
    let alice = create_in_group(&daemon, "alpine:1", "team-a");
    let bob = create_in_group(&daemon, "alpine:2", "team-b");

    let mut sub = daemon
        .cli()
        .args(["subscribe", &bob, "events"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn subscribe");
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Act
    daemon
        .cli()
        .args(["publish", &alice, "events", "hello"])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(200));

    sub.kill().expect("kill");
    let output = sub.wait_with_output().expect("wait_with_output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Assert: nothing arrived. The subscriber printed neither "payload:"
    // nor any from_sandbox line.
    assert!(
        !stdout.contains("payload:"),
        "subscriber must not see cross-group traffic, got: {stdout}"
    );
}

#[test]
fn given_user_passes_invalid_comms_mode_when_create_then_fails_locally() {
    // Arrange: clap parses successfully but the CLI's mode-string lookup
    // rejects unknown values BEFORE the wire. The error is local, so the
    // daemon never sees the bad request.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args([
            "create",
            "alpine",
            "--comms-mode",
            "bogus",
            "--comms-group",
            "team",
        ])
        .assert();

    // Assert: stderr names the offending field so users self-correct.
    assertion
        .failure()
        .stderr(predicate::str::contains("comms-mode"));
}
