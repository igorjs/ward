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
//!   - logs after exec emits a stdout line and an exit marker
//!   - logs with an invalid pid surfaces the validation error

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
// Feature: logs (streaming output)
// ---------------------------------------------------------------------------

#[test]
fn given_exec_when_user_runs_logs_then_streams_stdout_and_exit() {
    // Arrange: exec returns a pid. The stub backend deposits a single
    // stdout line and an Exit(0) event; `ward logs` drains both and
    // prints "stdout: <line>" and "exit: 0".
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");
    let exec_out = daemon
        .cli()
        .args(["exec", &id, "--", "echo", "hello"])
        .output()
        .expect("exec");
    let pid = extract_field(std::str::from_utf8(&exec_out.stdout).unwrap(), "pid: ");

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["logs", &id, &pid]).assert();

    // Assert: must succeed and stdout includes both the line prefix
    // ("stdout:") and the exit marker ("exit: 0"). The exact content of
    // the line is owned by the stub; tests assert the wire shape only.
    assertion
        .success()
        .stdout(predicate::str::contains("stdout:"))
        .stdout(predicate::str::contains("exit: 0"));
}

#[test]
fn given_invalid_pid_when_user_runs_logs_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: 'z' is not hex, so the entity_id validator rejects.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args([
            "logs",
            "00000000-0000-0000-0000-000000000000",
            "not-hex-zzz",
        ])
        .assert();

    // Assert: non-zero exit, stderr names the offending field so the
    // user knows what to fix without reading docs.
    assertion
        .failure()
        .stderr(predicate::str::contains("process"));
}

#[test]
fn given_unknown_pid_when_user_runs_logs_then_fails_with_not_found() {
    // Arrange: well-formed pid that no exec ever produced.
    let daemon = common::Daemon::spawn();
    let unknown_pid = "00000000-0000-0000-0000-000000000000";

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["logs", unknown_pid, unknown_pid]).assert();

    // Assert: NotFound (not Internal). stderr should echo the offending
    // pid for grep-friendly CI logs.
    assertion
        .failure()
        .stderr(predicate::str::contains(unknown_pid));
}

// ---------------------------------------------------------------------------
// Feature: stdin
// ---------------------------------------------------------------------------

#[test]
fn given_exec_when_user_writes_stdin_then_command_succeeds() {
    // Arrange: exec returns a pid whose stdin channel is held alive by
    // the stub's drain task — so a write succeeds even though the stub
    // process has already finished its scripted output.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");
    let exec_out = daemon
        .cli()
        .args(["exec", &id, "--", "cat"])
        .output()
        .expect("exec");
    let pid = extract_field(std::str::from_utf8(&exec_out.stdout).unwrap(), "pid: ");

    // Act: literal-arg form — avoids touching the CLI's own stdin in tests.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["stdin", &id, &pid, "hello\n"]).assert();

    // Assert: command succeeds and prints the "wrote" confirmation.
    assertion
        .success()
        .stdout(predicate::str::contains("wrote"));
}

#[test]
fn given_invalid_pid_when_user_writes_stdin_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args([
            "stdin",
            "00000000-0000-0000-0000-000000000000",
            "not-hex-zzz",
            "x",
        ])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("process"));
}

#[test]
fn given_unknown_pid_when_user_writes_stdin_then_fails_with_not_found() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let unknown_pid = "00000000-0000-0000-0000-000000000000";

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["stdin", unknown_pid, unknown_pid, "x"])
        .assert();

    // Assert: stderr echoes the offending pid so users can grep CI logs.
    assertion
        .failure()
        .stderr(predicate::str::contains(unknown_pid));
}
