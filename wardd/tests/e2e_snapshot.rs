// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward snapshot create`, `ward snapshot restore`,
//! and `ward snapshot list`.
//!
//! Simulates a user typing `ward snapshot …` against a real wardd
//! subprocess. The snapshot backend is unimplemented today, so the
//! happy-path scenarios assert that the daemon returns Unimplemented
//! and the CLI surfaces it on stderr. The negative scenarios assert
//! that malformed inputs are rejected at the validation boundary
//! with InvalidArgument BEFORE reaching the unimplemented stub —
//! that contract is durable across the backend implementation.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

const VALID_UUID: &str = "00000000-0000-0000-0000-000000000000";

// ---------------------------------------------------------------------------
// Feature: snapshot create
// ---------------------------------------------------------------------------

#[test]
fn given_valid_sandbox_id_when_user_runs_snapshot_create_then_fails_with_unimplemented() {
    // Arrange: any well-formed sandbox UUID works — the daemon returns
    // Unimplemented after validation passes, regardless of whether the
    // sandbox exists.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "create", VALID_UUID, "--label", "checkpoint-1"])
        .assert();

    // Assert: when the backend lands, this assertion is the one that
    // changes — flip from Unimplemented to a successful stdout match.
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
}

#[test]
fn given_malformed_sandbox_id_when_user_runs_snapshot_create_then_fails_with_invalid_argument() {
    // Arrange: 'z' is not hex, so entity_id rejects the sandbox_id.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "create", "not-a-uuid-zzz", "--label", "x"])
        .assert();

    // Assert: stderr mentions the offending field ("sandbox") so users
    // can fix the typo without consulting docs.
    assertion
        .failure()
        .stderr(predicate::str::contains("sandbox"));
}

// ---------------------------------------------------------------------------
// Feature: snapshot restore
// ---------------------------------------------------------------------------

#[test]
fn given_valid_ids_when_user_runs_snapshot_restore_then_fails_with_unimplemented() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["snapshot", "restore", VALID_UUID, VALID_UUID])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
}

#[test]
fn given_malformed_snapshot_id_when_user_runs_snapshot_restore_then_fails_with_invalid_argument() {
    // Arrange: valid sandbox_id, malformed snapshot_id — exercises the
    // second validator in restore (regression guard for the order in
    // which the two entity_id calls happen).
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
fn given_valid_sandbox_id_when_user_runs_snapshot_list_then_fails_with_unimplemented() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["snapshot", "list", VALID_UUID]).assert();

    // Assert: when the backend lands, this becomes an empty-stdout
    // assertion (the convention for "no rows found" elsewhere in the CLI).
    assertion
        .failure()
        .stderr(predicate::str::contains("Unimplemented"));
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
