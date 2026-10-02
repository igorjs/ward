// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E regression: a same-UID, different-process caller that only knows a
//! `sandbox_id` cannot act on a sandbox it did not create, while the CLI's
//! own two-process `create` then `exec` workflow keeps working.
//!
//! Each scenario runs against a real `wardd` subprocess and a real `ward`
//! CLI subprocess, mirroring how a user's two separate shell invocations
//! would behave: one process's `ward create` writes a capability token to
//! the on-disk store; a later `ward <subcommand>` invocation reads it back.
//! Simulating "a process that never saw the create response" means editing
//! that store file directly between invocations, bypassing the CLI's own
//! `tokens::save` so the daemon's wire-level check is what is proven, not
//! just "the CLI never sent a header".

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::prelude::*;
use predicates::prelude::*;

/// Parse a `<prefix><value>` line out of a CLI command's stdout.
fn extract_field(stdout: &str, prefix: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    panic!("no {prefix:?} line in stdout:\n{stdout}");
}

/// The CLI derives its token store path as a sibling of the daemon socket
/// (see `ward-cli/src/tokens.rs::store_path`). Tests reach into that same
/// file directly to simulate a second process's view of the store.
fn token_store_path(daemon: &common::Daemon) -> PathBuf {
    daemon
        .socket
        .parent()
        .expect("socket has a parent dir")
        .join("tokens.json")
}

/// Overwrite the token store with an empty map, as if a fresh process had
/// never received a `create` response for any sandbox.
fn clear_token_store(store: &Path) {
    fs::write(store, "{}").expect("clear token store");
}

/// Hand-write a single sandbox_id -> token entry into the store file,
/// bypassing `tokens::save` entirely so the daemon's own check is exercised
/// rather than the CLI's decision to attach a header.
fn hand_write_token(store: &Path, sandbox_id: &str, token: &str) {
    fs::write(store, format!("{{\"{sandbox_id}\":\"{token}\"}}")).expect("hand-write token");
}

// ---------------------------------------------------------------------------
// Scenario 1: a second process with no stored token cannot act on a
// sandbox it did not create.
// ---------------------------------------------------------------------------

#[test]
fn given_two_separate_cli_processes_when_second_has_no_stored_token_then_denied() {
    // Arrange: one "process" creates the sandbox, persisting its token.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(create_out.status.success());
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // A separate process never saw that create response, so its view of
    // the store is empty.
    clear_token_store(&token_store_path(&daemon));

    // Act: that second process tries to exec into the sandbox by id alone.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", &id, "--", "echo", "hi"]).assert();

    // Assert: denied, and the message names the sandbox so an operator can
    // tell which resource was involved.
    assertion
        .failure()
        .stderr(predicate::str::contains("permission"))
        .stderr(predicate::str::contains(&id));
}

// ---------------------------------------------------------------------------
// Scenario 2: the normal two-process CLI workflow (create, then exec in a
// fresh invocation against the same persisted store) still succeeds.
// ---------------------------------------------------------------------------

#[test]
fn given_create_then_exec_in_new_process_when_token_persisted_then_succeeds() {
    // Arrange: `ward create` in one invocation.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(create_out.status.success());
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act: `ward exec` in a second, independent invocation against the
    // same token store, exactly as a user's two shell commands would.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", &id, "--", "echo", "hi"]).assert();

    // Assert: the persisted token authorizes the second process.
    assertion
        .success()
        .stdout(predicate::str::contains("pid:"))
        .stdout(predicate::str::contains("status: running"));
}

// ---------------------------------------------------------------------------
// Scenario 3: a hand-crafted wrong token is rejected on the wire, not just
// "the CLI never sent one".
// ---------------------------------------------------------------------------

#[test]
fn given_hand_crafted_wrong_token_when_exec_then_permission_denied() {
    // Arrange: create a sandbox, then overwrite the store's entry with a
    // garbage value directly, bypassing `tokens::save`.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(create_out.status.success());
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    hand_write_token(&token_store_path(&daemon), &id, "totally-wrong-token");

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", &id, "--", "echo", "hi"]).assert();

    // Assert: the daemon rejects the wrong token itself.
    assertion
        .failure()
        .stderr(predicate::str::contains("permission"))
        .stderr(predicate::str::contains(&id));
}

// ---------------------------------------------------------------------------
// Scenario 4: restoring from another sandbox's snapshot requires that
// sandbox's own stored token, end to end through the real CLI and daemon.
// ---------------------------------------------------------------------------

#[test]
fn given_create_sandbox_from_snapshot_without_source_token_when_called_then_permission_denied() {
    // Arrange: create sandbox A and snapshot it.
    let daemon = common::Daemon::spawn();
    let create_out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(create_out.status.success());
    let source_id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    let snapshot_out = daemon
        .cli()
        .args(["snapshot", "create", &source_id, "--label", "checkpoint"])
        .output()
        .expect("snapshot create");
    assert!(snapshot_out.status.success());
    let snapshot_id = extract_field(
        std::str::from_utf8(&snapshot_out.stdout).unwrap(),
        "snapshot_id: ",
    );

    // A separate process with no stored token for sandbox A.
    clear_token_store(&token_store_path(&daemon));

    // Act: try to create a new sandbox restored from A's snapshot without
    // A's token to authorize it.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args([
            "create",
            "alpine:latest",
            "--from-snapshot",
            &snapshot_id,
            "--source-sandbox",
            &source_id,
        ])
        .assert();

    // Assert: denied, naming the source sandbox whose token was missing.
    assertion
        .failure()
        .stderr(predicate::str::contains("permission"))
        .stderr(predicate::str::contains(&source_id));
}
