// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward volume create / list / remove`.
//!
//! Simulates a user typing `ward volume ...` at a shell against a real
//! wardd subprocess. Asserts on stdout / stderr / exit codes — exactly
//! what the user sees. This catches output-formatting and exit-code bugs
//! that in-process gRPC tests cannot.
//!
//! Style: BDD names with AAA bodies, one scenario per test. The shared
//! `common::Daemon` guard provides per-test isolation.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

// ---------------------------------------------------------------------------
// Helper: parse the ID line from `ward volume create` output.
//
// `ward volume create` prints "id: <uuid>" on the first line. Several
// scenarios need that ID for subsequent commands.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn extract_id(stdout: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("id: ") {
            return rest.trim().to_string();
        }
    }
    panic!("no `id:` line in stdout:\n{stdout}");
}

// ---------------------------------------------------------------------------
// Feature: volume create
// ---------------------------------------------------------------------------

// mkfs.ext4 is Linux-only with no usable macOS port. Tests that go through
// `ward volume create` need the formatter to succeed, so gate them to Linux.
// Sibling tests that only exercise list-empty, name validation, or unknown-id
// remove paths stay un-gated.
#[cfg(target_os = "linux")]
#[test]
fn given_running_daemon_when_user_creates_volume_then_command_succeeds_and_prints_id() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: the user types `ward volume create demo --size 100`.
    let mut cmd = daemon.cli();
    let output = cmd
        .args(["volume", "create", "demo", "--size", "100"])
        .assert();

    // Assert: success + the four expected output lines so scripts can grep.
    output
        .success()
        .stdout(predicate::str::contains("id:"))
        .stdout(predicate::str::contains("name: demo"))
        .stdout(predicate::str::contains("size_mb: 100"))
        .stdout(predicate::str::contains("mount_path:"));
}

#[test]
fn given_running_daemon_when_user_creates_volume_with_invalid_name_then_fails_with_clear_error() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: shell users will sometimes try `volume create "my volume"`.
    let mut cmd = daemon.cli();
    let output = cmd
        .args(["volume", "create", "my volume", "--size", "100"])
        .assert();

    // Assert: non-zero exit and stderr mentions the offending field so
    // the user knows what to fix without reading source code.
    output
        .failure()
        .stderr(predicate::str::contains("volume name"));
}

// ---------------------------------------------------------------------------
// Feature: volume list
// ---------------------------------------------------------------------------

#[test]
fn given_no_volumes_when_user_runs_list_then_output_is_empty() {
    // Arrange: a daemon with zero volumes (fresh state).
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let output = cmd.args(["volume", "list"]).assert();

    // Assert: command exits 0 and prints nothing. Empty output is the
    // contract for "no matches" — distinct from error (non-zero exit).
    output.success().stdout(predicate::str::is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn given_two_created_volumes_when_user_runs_list_then_both_names_appear() {
    // Arrange
    let daemon = common::Daemon::spawn();
    daemon
        .cli()
        .args(["volume", "create", "alpha", "--size", "100"])
        .assert()
        .success();
    daemon
        .cli()
        .args(["volume", "create", "beta", "--size", "200"])
        .assert()
        .success();

    // Act
    let mut cmd = daemon.cli();
    let output = cmd.args(["volume", "list"]).assert();

    // Assert: both names show up in the tab-separated output. We do not
    // pin column ordering (HashMap iteration is non-deterministic).
    output
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"));
}

// ---------------------------------------------------------------------------
// Feature: volume remove
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn given_created_volume_when_user_removes_it_then_list_no_longer_shows_it() {
    // Arrange: create one volume, capture its ID.
    let daemon = common::Daemon::spawn();
    let create_output = daemon
        .cli()
        .args(["volume", "create", "ephemeral", "--size", "100"])
        .output()
        .expect("create");
    assert!(create_output.status.success());
    let id = extract_id(std::str::from_utf8(&create_output.stdout).unwrap());

    // Act: remove the volume.
    daemon
        .cli()
        .args(["volume", "remove", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed:"));

    // Assert: it no longer appears in `ward volume list`.
    daemon
        .cli()
        .args(["volume", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&id).not());
}

#[test]
fn given_unknown_id_when_user_removes_volume_then_fails_with_not_found() {
    // Arrange: a syntactically-valid UUID the daemon has never seen.
    let daemon = common::Daemon::spawn();
    let unknown_id = "00000000-0000-0000-0000-000000000000";

    // Act
    let mut cmd = daemon.cli();
    let output = cmd.args(["volume", "remove", unknown_id]).assert();

    // Assert: non-zero exit and the error mentions the offending ID.
    output
        .failure()
        .stderr(predicate::str::contains(unknown_id));
}

// ---------------------------------------------------------------------------
// Feature: capacity cap
// ---------------------------------------------------------------------------
//
// The daemon defaults to max_volumes = 256, too many for an E2E test.
// We override via the env var to make the cap exercise practical.

#[cfg(target_os = "linux")]
#[test]
fn given_max_volumes_is_two_when_user_creates_third_then_fails_with_limit_message() {
    // Arrange: spawn a daemon configured with a tiny volume cap by setting
    // WARD_MAX_VOLUMES before spawn. We use the standard harness then
    // override the env via a custom spawn here.

    let data_dir = tempfile::tempdir().expect("tempdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = std::process::Command::cargo_bin("wardd")
        .unwrap()
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_MAX_VOLUMES", "2")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn wardd");

    // Wait for the socket.
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

    // Fill to cap.
    ward()
        .args(["volume", "create", "v1", "--size", "10"])
        .assert()
        .success();
    ward()
        .args(["volume", "create", "v2", "--size", "10"])
        .assert()
        .success();

    // Act: third must fail.
    let assertion = ward()
        .args(["volume", "create", "v3", "--size", "10"])
        .assert();

    // Assert: non-zero exit, message mentions "limit" so users grep CI logs.
    assertion
        .failure()
        .stderr(predicate::str::contains("limit"));

    // Teardown: stop the daemon and reap so the test does not leak.
    let _ = wardd.kill();
    let _ = wardd.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn given_volume_cap_reached_when_user_removes_one_then_create_succeeds_again() {
    // Arrange: bespoke spawn with a tiny cap, fill it, then exercise the
    // recovery path. The integration tier already covers this; the E2E
    // tier locks in the equivalent flow through the CLI surface — slot
    // accounting bugs that only show up across the wire are caught here.
    let data_dir = tempfile::tempdir().expect("tempdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = std::process::Command::cargo_bin("wardd")
        .unwrap()
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_MAX_VOLUMES", "2")
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

    // Fill the cap and capture the first volume id for removal.
    let first = ward()
        .args(["volume", "create", "v1", "--size", "10"])
        .output()
        .expect("create");
    assert!(first.status.success());
    let first_id = std::str::from_utf8(&first.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("id: ").map(str::trim))
        .expect("id line")
        .to_string();

    ward()
        .args(["volume", "create", "v2", "--size", "10"])
        .assert()
        .success();
    ward()
        .args(["volume", "create", "v3", "--size", "10"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("limit"));

    // Act: free a slot and retry.
    ward()
        .args(["volume", "remove", &first_id])
        .assert()
        .success();
    let assertion = ward()
        .args(["volume", "create", "replacement", "--size", "10"])
        .assert();

    // Assert
    assertion.success().stdout(predicate::str::contains("id: "));

    let _ = wardd.kill();
    let _ = wardd.wait();
}
