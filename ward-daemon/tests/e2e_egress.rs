// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward egress suggest` against a stub daemon. The stub backend has no
//! policed network, so nothing is ever recorded; these tests cover the RPC
//! path, token checks and `--apply` wording. A real recording is covered in
//! `e2e_vm.rs`.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn create_sandbox(daemon: &common::Daemon) -> String {
    let out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(out.status.success());
    std::str::from_utf8(&out.stdout).unwrap().trim().to_string()
}

#[test]
fn given_sandbox_with_nothing_blocked_when_suggest_then_prints_no_flags() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);

    // Act
    let assertion = daemon.cli().args(["egress", "suggest", &id]).assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("No blocked destinations"));
}

#[test]
fn given_apply_when_suggest_then_fails_with_clear_message() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);

    // Act
    let assertion = daemon
        .cli()
        .args(["egress", "suggest", &id, "--apply"])
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("--apply is not supported"));
}

#[test]
fn given_no_stored_token_when_suggest_then_denied() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);
    let store = daemon.socket.parent().unwrap().join("tokens.json");
    std::fs::write(&store, "{}").expect("clear token store");

    // Act
    let assertion = daemon.cli().args(["egress", "suggest", &id]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("token does not match"));
}
