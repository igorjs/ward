// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward snapshot create`, `ward snapshot restore`,
//! and `ward snapshot list`.
//!
//! Simulates a user typing `ward snapshot …` against a real wardd
//! subprocess. The snapshot backend is currently a stub (metadata-only;
//! real libkrun checkpoint integration lands later) but the wire and
//! CLI contracts hold: success on happy paths, NotFound on unknown ids,
//! InvalidArgument on malformed ids.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

const VALID_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// Parse `id: <value>` out of stdout for chained commands.
fn extract_field(stdout: &str, prefix: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    panic!("no {prefix:?} line in stdout:\n{stdout}");
}

// ---------------------------------------------------------------------------
// Feature: snapshot create
// ---------------------------------------------------------------------------

#[test]
fn given_existing_sandbox_when_user_runs_snapshot_create_then_prints_snapshot_id() {
    // Arrange: create a sandbox so the snapshot has a parent.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "create", &id, "--label", "checkpoint-1"])
        .assert();

    // Assert: per-line fields scripts can grep, including the round-tripped
    // label.
    assertion
        .success()
        .stdout(predicate::str::contains("snapshot_id:"))
        .stdout(predicate::str::contains(format!("sandbox_id: {id}")))
        .stdout(predicate::str::contains("label: checkpoint-1"));
}

#[test]
fn given_unknown_sandbox_when_user_runs_snapshot_create_then_fails_with_not_found() {
    // Arrange: well-formed UUID, no matching sandbox.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "create", VALID_UUID, "--label", "x"])
        .assert();

    // Assert: non-zero exit; stderr mentions the offending sandbox id.
    assertion
        .failure()
        .stderr(predicate::str::contains(VALID_UUID));
}

#[test]
fn given_malformed_sandbox_id_when_user_runs_snapshot_create_then_fails_with_invalid_argument() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "create", "not-a-uuid-zzz", "--label", "x"])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("sandbox"));
}

// ---------------------------------------------------------------------------
// Feature: snapshot restore
// ---------------------------------------------------------------------------

#[test]
fn given_existing_snapshot_when_user_runs_snapshot_restore_then_prints_restored() {
    // Arrange: create + snapshot, then capture the snapshot id.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");
    let snap_out = daemon
        .cli()
        .args(["snapshot", "create", &id, "--label", "before"])
        .output()
        .expect("snapshot create");
    let snap_id = extract_field(
        std::str::from_utf8(&snap_out.stdout).unwrap(),
        "snapshot_id: ",
    );

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["snapshot", "restore", &id, &snap_id]).assert();

    // Assert: the CLI's "restored:" confirmation line surfaces both ids
    // so the user can grep for the operation.
    assertion.success().stdout(predicate::str::contains(format!(
        "restored: {id} from {snap_id}"
    )));
}

#[test]
fn given_unknown_snapshot_when_user_runs_snapshot_restore_then_fails_with_not_found() {
    // Arrange: real sandbox but the snapshot id is unknown.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["snapshot", "restore", &id, VALID_UUID]).assert();

    // Assert: stderr names the snapshot (not sandbox) so users investigate
    // the right thing.
    assertion
        .failure()
        .stderr(predicate::str::contains("snapshot"));
}

#[test]
fn given_malformed_snapshot_id_when_user_runs_snapshot_restore_then_fails_with_invalid_argument() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "restore", VALID_UUID, "not-hex-zzz"])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("snapshot"));
}

// ---------------------------------------------------------------------------
// Feature: snapshot list
// ---------------------------------------------------------------------------

#[test]
fn given_no_snapshots_when_user_runs_snapshot_list_then_stdout_is_empty() {
    // Arrange: brand-new sandbox with no snapshots.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act
    let out = daemon
        .cli()
        .args(["snapshot", "list", &id])
        .output()
        .expect("snapshot list");

    // Assert: success exit, empty stdout. The convention across the CLI
    // is "no rows = empty output"; callers distinguish "no snapshots"
    // from "command failed" via the exit code.
    assert!(out.status.success(), "list should succeed: {:?}", out);
    assert!(
        out.stdout.is_empty(),
        "expected empty stdout: {:?}",
        out.stdout
    );
}

#[test]
fn given_two_snapshots_when_user_runs_snapshot_list_then_both_appear() {
    // Arrange: two snapshots from the same sandbox.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine"])
        .output()
        .expect("create");
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");
    daemon
        .cli()
        .args(["snapshot", "create", &id, "--label", "first"])
        .assert()
        .success();
    daemon
        .cli()
        .args(["snapshot", "create", &id, "--label", "second"])
        .assert()
        .success();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["snapshot", "list", &id]).assert();

    // Assert: stdout contains both labels — tab-separated columns format,
    // so a substring match is sufficient.
    assertion
        .success()
        .stdout(predicate::str::contains("first"))
        .stdout(predicate::str::contains("second"));
}

#[test]
fn given_malformed_sandbox_id_when_user_runs_snapshot_list_then_fails_with_invalid_argument() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["snapshot", "list", "not-a-uuid-zzz"]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("sandbox"));
}
