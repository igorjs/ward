// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward exec`, `ward run`, and `ward logs`.
//!
//! Simulates a user typing commands at a shell against a real wardd
//! subprocess. Real VM execution is gated behind the krunvm feature;
//! the stub backend returns synthetic pids. Today these tests cover:
//!
//!   - exec with valid args returns a pid the user can pass to `logs`
//!   - exec with invalid args returns a clear validation error
//!   - run for a supported language returns a pid
//!   - run for cobol (unsupported) returns a clear error
//!   - logs against the stub returns Unimplemented
//!
//! When streaming lands, this file expands with positive log scenarios.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helpers: parse `id:` and `pid:` lines out of stdout for chained commands.
// ---------------------------------------------------------------------------

fn extract_field(stdout: &str, prefix: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    panic!("no {prefix:?} line in stdout:\n{stdout}");
}

// ---------------------------------------------------------------------------
// Feature: exec
// ---------------------------------------------------------------------------

#[test]
fn given_existing_sandbox_when_user_runs_exec_echo_then_returns_pid_and_running_status() {
    // Arrange: create the sandbox first; exec needs a target.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(create_out.status.success());
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act: the user types `ward exec <id> -- echo hello`.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", &id, "--", "echo", "hello"]).assert();

    // Assert: success plus pid + status fields scripts can grep.
    assertion
        .success()
        .stdout(predicate::str::contains("pid:"))
        .stdout(predicate::str::contains("status: running"));
}

#[test]
fn given_unknown_sandbox_when_user_runs_exec_then_fails_with_not_found() {
    // Arrange: well-formed UUID with no matching sandbox.
    let daemon = common::Daemon::spawn();
    let unknown_id = "00000000-0000-0000-0000-000000000000";

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", unknown_id, "--", "echo", "hi"]).assert();

    // Assert: non-zero exit; stderr mentions the offending sandbox id
    // (the gRPC NotFound message embeds it for grep-ability).
    assertion
        .failure()
        .stderr(predicate::str::contains(unknown_id));
}

// ---------------------------------------------------------------------------
// Feature: run
// ---------------------------------------------------------------------------

#[test]
fn given_existing_sandbox_when_user_runs_python_snippet_then_returns_pid() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "python:3.12-slim"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["run", &id, "--language", "python", "--code", "print('hi')"])
        .assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::contains("pid:"))
        .stdout(predicate::str::contains("status: running"));
}

#[test]
fn given_unsupported_language_when_user_runs_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act: "cobol" is not in the runtime table.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["run", &id, "--language", "cobol", "--code", "DISPLAY 'hi'"])
        .assert();

    // Assert: non-zero exit and stderr mentions the offending language.
    assertion
        .failure()
        .stderr(predicate::str::contains("cobol"));
}

#[test]
fn given_invalid_language_name_when_user_runs_then_fails() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act: dashes in language names fail the validator regex.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["run", &id, "--language", "py-thon", "--code", "print(1)"])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("language"));
}

// ---------------------------------------------------------------------------
// Feature: logs (streaming output, currently unimplemented)
// ---------------------------------------------------------------------------

#[test]
fn given_valid_log_request_when_user_runs_logs_then_fails_with_unimplemented() {
    // Arrange: any valid IDs work — StreamOutput is unimplemented at the
    // daemon, so the response is the same regardless of values.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["logs", "deadbeef", "deadbeef"]).assert();

    // Assert: non-zero exit and stderr says Unimplemented. When streaming
    // lands, this test gets rewritten to assert on real stdout output;
    // the negative-input tests above keep their shape.
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
}
