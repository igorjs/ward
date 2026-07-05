// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward create / list / remove` against a real wardd
//! subprocess. Simulates a user typing commands at a shell; assertions
//! target stdout / stderr / exit codes.
//!
//! Style: BDD names with AAA bodies, one scenario per test. The shared
//! `common::Daemon` guard provides per-test isolation.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helper: parse `id: <uuid>` out of `ward create` stdout for use in later
// commands (`ward remove <id>`).
// ---------------------------------------------------------------------------

fn extract_id(stdout: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("id: ") {
            return rest.trim().to_string();
        }
    }
    panic!("no `id:` line in stdout:\n{stdout}");
}

// ---------------------------------------------------------------------------
// Feature: sandbox create
// ---------------------------------------------------------------------------

#[test]
fn given_running_daemon_when_user_creates_sandbox_then_command_succeeds_and_prints_id() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: the user types `ward create alpine:latest`.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["create", "alpine:latest"]).assert();

    // Assert: exit 0 plus the three required output lines so scripts can
    // grep individual fields without parsing structured output.
    assertion
        .success()
        .stdout(predicate::str::contains("id:"))
        .stdout(predicate::str::contains("status: creating"))
        .stdout(predicate::str::contains("image: alpine:latest"));
}

#[test]
fn given_running_daemon_when_user_creates_with_invalid_image_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: path-traversal sequences in image references must be rejected
    // at the validator before they ever reach the backend.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["create", "../escape"]).assert();

    // Assert: non-zero exit and stderr mentions the offending field so
    // the user knows what to fix without reading source code.
    assertion
        .failure()
        .stderr(predicate::str::contains("image reference"));
}

#[test]
fn given_running_daemon_when_user_creates_with_oversized_cpus_then_fails() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: 9999 cpus exceeds the validator's MAX_CPUS=64 cap.
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["create", "alpine:latest", "--cpus", "9999"])
        .assert();

    // Assert: rejected before allocation.
    assertion.failure().stderr(predicate::str::contains("cpus"));
}

// ---------------------------------------------------------------------------
// Feature: sandbox list
// ---------------------------------------------------------------------------

#[test]
fn given_no_sandboxes_when_user_runs_list_then_output_is_empty() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("list").assert();

    // Assert: empty stdout is the contract for "no matches" — distinct
    // from "command failed" which would be a non-zero exit.
    assertion.success().stdout(predicate::str::is_empty());
}

#[test]
fn given_two_created_sandboxes_when_user_runs_list_then_both_images_appear() {
    // Arrange
    let daemon = common::Daemon::spawn();
    daemon.cli().args(["create", "alpine:a"]).assert().success();
    daemon.cli().args(["create", "alpine:b"]).assert().success();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("list").assert();

    // Assert: both images appear in the tab-separated output. We do not
    // pin row ordering; HashMap iteration is non-deterministic.
    assertion
        .success()
        .stdout(predicate::str::contains("alpine:a"))
        .stdout(predicate::str::contains("alpine:b"));
}

// ---------------------------------------------------------------------------
// Feature: sandbox remove
// ---------------------------------------------------------------------------

#[test]
fn given_created_sandbox_when_user_removes_it_then_list_no_longer_shows_it() {
    // Arrange: create one, capture its ID via stdout parsing.
    let daemon = common::Daemon::spawn();
    let create_output = daemon
        .cli()
        .args(["create", "alpine:demo"])
        .output()
        .expect("create");
    assert!(create_output.status.success());
    let id = extract_id(std::str::from_utf8(&create_output.stdout).unwrap());

    // Act
    daemon
        .cli()
        .args(["remove", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed:"));

    // Assert: list no longer mentions the ID.
    daemon
        .cli()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains(&id).not());
}

#[test]
fn given_unknown_id_when_user_removes_sandbox_then_fails_with_not_found() {
    // Arrange: well-formed UUID the daemon has never seen.
    let daemon = common::Daemon::spawn();
    let unknown_id = "00000000-0000-0000-0000-000000000000";

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["remove", unknown_id]).assert();

    // Assert: non-zero exit and stderr mentions the offending ID so the
    // user knows what they typed wrong without reading logs.
    assertion
        .failure()
        .stderr(predicate::str::contains(unknown_id));
}

// ---------------------------------------------------------------------------
// Feature: capacity cap
// ---------------------------------------------------------------------------

#[test]
fn given_max_sandboxes_is_two_when_user_creates_third_then_fails_with_limit_message() {
    // Arrange: spawn a daemon with a tiny cap so we can hit it in three
    // commands. Uses a one-off subprocess spawn instead of the shared
    // harness because we need to override an env var the harness does
    // not parameterise.
    let data_dir = tempfile::tempdir().expect("tempdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = std::process::Command::cargo_bin("wardd")
        .unwrap()
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_MAX_SANDBOXES", "2")
        .env("WARD_OCI_OFFLINE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wardd");

    // Wait for the socket to appear.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !socket.exists() {
        let _ = wardd.kill();
        let _ = wardd.wait();
        panic!("daemon did not bind");
    }

    let ward = || {
        let mut cmd = assert_cmd::Command::cargo_bin("ward").unwrap();
        cmd.env("WARD_SOCKET", &socket);
        cmd
    };

    // Fill the cap.
    ward().args(["create", "alpine:1"]).assert().success();
    ward().args(["create", "alpine:2"]).assert().success();

    // Act: third must fail.
    let assertion = ward().args(["create", "alpine:3"]).assert();

    // Assert: non-zero exit and stderr mentions "limit" so users grep
    // their CI logs and know what changed.
    assertion
        .failure()
        .stderr(predicate::str::contains("limit"));

    // Teardown: reap the daemon explicitly so the test does not leak a
    // zombie if it panics later.
    let _ = wardd.kill();
    let _ = wardd.wait();
}

#[test]
fn given_sandbox_cap_reached_when_user_removes_one_then_create_succeeds_again() {
    // Arrange: same bespoke spawn shape as the cap-hit test above. The
    // unit + integration tiers already prove this via direct calls; this
    // E2E layer locks in the full user-visible flow (CLI → wardd → manager)
    // so a regression in slot accounting is caught at the contract that
    // ships to users.
    let data_dir = tempfile::tempdir().expect("tempdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = std::process::Command::cargo_bin("wardd")
        .unwrap()
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_MAX_SANDBOXES", "2")
        .env("WARD_OCI_OFFLINE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wardd");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !socket.exists() {
        let _ = wardd.kill();
        let _ = wardd.wait();
        panic!("daemon did not bind");
    }

    let ward = || {
        let mut cmd = assert_cmd::Command::cargo_bin("ward").unwrap();
        cmd.env("WARD_SOCKET", &socket);
        cmd
    };

    // Fill the cap and capture the first sandbox id for removal.
    let first = ward()
        .args(["create", "alpine:1"])
        .output()
        .expect("create");
    assert!(first.status.success());
    let first_id = std::str::from_utf8(&first.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("id: ").map(str::trim))
        .expect("id line")
        .to_string();

    ward().args(["create", "alpine:2"]).assert().success();
    ward()
        .args(["create", "alpine:3"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("limit"));

    // Act: remove the first and try again.
    ward().args(["remove", &first_id]).assert().success();
    let assertion = ward().args(["create", "alpine:replacement"]).assert();

    // Assert: success — the removed slot must be available to a new create.
    assertion.success().stdout(predicate::str::contains("id: "));

    let _ = wardd.kill();
    let _ = wardd.wait();
}
