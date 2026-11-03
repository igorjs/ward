// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward wait --condition` with the stub backend.

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::Daemon;

const WAITER_HEAD_START: Duration = Duration::from_millis(500);
const CLAP_USAGE_EXIT: i32 = 2;

fn create(d: &Daemon) {
    let out = d
        .cli()
        .args(["create", "--name", "web1", "alpine:latest"])
        .output()
        .expect("create");
    assert!(out.status.success());
}

#[test]
fn given_a_sandbox_when_waited_for_removed_then_the_wait_ends_after_rm() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);
    let waiter = d
        .cli()
        .args(["wait", "--condition", "removed", "web1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wait");
    std::thread::sleep(WAITER_HEAD_START);

    // Act
    let removed = d.cli().args(["rm", "-f", "web1"]).output().expect("rm");
    let out = waiter.wait_with_output().expect("wait");

    // Assert
    assert!(removed.status.success());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn given_a_stopped_sandbox_when_waited_with_the_default_condition_then_it_returns_at_once() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);
    d.cli().args(["stop", "web1"]).output().expect("stop");

    // Act
    let out = d.cli().args(["wait", "web1"]).output().expect("wait");

    // Assert
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn given_an_unknown_condition_when_parsed_then_clap_rejects_it() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = d
        .cli()
        .args(["wait", "--condition", "later", "web1"])
        .output()
        .expect("wait");

    // Assert
    assert_eq!(out.status.code(), Some(CLAP_USAGE_EXIT));
}
