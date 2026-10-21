// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward stop / start / restart / rm / prune` against a
//! real wardd subprocess with the stub backend.
//!
//! Style: BDD names with AAA bodies, one scenario per test.

mod common;

use std::process::Output;

use common::Daemon;
use predicates::prelude::*;

/// Run the CLI and return its raw output without asserting on the exit code.
fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Create a sandbox and return its id.
fn create(d: &Daemon, extra: &[&str]) -> String {
    let out = run(d, &[&["create"], extra, &["alpine:latest"]].concat());
    assert!(out.status.success(), "create failed: {}", stderr(&out));
    stdout(&out)
        .lines()
        .find_map(|l| l.strip_prefix("id: "))
        .expect("id line")
        .trim()
        .to_string()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    stdout(&out)
}

/// The status column of `ward list` for `id`, or None when it is not listed.
fn status_of(d: &Daemon, id: &str) -> Option<String> {
    ok(d, &["list"]).lines().find_map(|l| {
        let mut cols = l.split('\t');
        (cols.next() == Some(id)).then(|| cols.next().unwrap_or_default().to_string())
    })
}

/// Drop `id` from the CLI token store, as if the sandbox came from elsewhere.
fn forget_token(d: &Daemon, id: &str) {
    let store = d.socket.parent().expect("socket dir").join("tokens.json");
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&store).expect("read store"))
            .expect("parse store");
    map.remove(id).expect("token was stored");
    std::fs::write(&store, serde_json::to_string(&map).expect("encode")).expect("write store");
}

#[test]
fn given_sandbox_named_web1_when_stop_web1_then_it_stops_and_prints_the_name() {
    // Arrange: the token is stored under the id, not the name
    let d = Daemon::spawn();
    let id = create(&d, &["--name", "web1"]);

    // Act
    let printed = ok(&d, &["stop", "web1"]);

    // Assert
    assert_eq!(printed.trim(), "web1");
    assert_eq!(status_of(&d, &id).as_deref(), Some("stopped"));
}

#[test]
fn given_id_prefix_when_stop_then_sandbox_stops() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    ok(&d, &["stop", &id[..8]]);

    // Assert
    assert_eq!(status_of(&d, &id).as_deref(), Some("stopped"));
}

#[test]
fn given_stopped_sandbox_when_start_then_it_runs_again() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);
    ok(&d, &["stop", &id]);

    // Act
    let printed = ok(&d, &["start", &id]);

    // Assert
    assert_eq!(printed.trim(), id);
    assert_eq!(status_of(&d, &id).as_deref(), Some("running"));
}

#[test]
fn given_running_sandbox_when_restart_then_it_is_running_again() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    let printed = ok(&d, &["restart", &id]);

    // Assert
    assert_eq!(printed.trim(), id);
    assert_eq!(status_of(&d, &id).as_deref(), Some("running"));
}

#[test]
fn given_two_sandboxes_when_stop_both_then_each_reference_is_printed() {
    // Arrange
    let d = Daemon::spawn();
    let a = create(&d, &[]);
    let b = create(&d, &[]);

    // Act
    let printed = ok(&d, &["stop", &a, &b]);

    // Assert
    assert_eq!(
        printed.lines().collect::<Vec<_>>(),
        [a.as_str(), b.as_str()]
    );
}

#[test]
fn given_one_unknown_reference_when_stop_then_exit_is_nonzero_and_the_others_stop() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    let out = run(&d, &["stop", "nosuch", &id]);

    // Assert
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nosuch"), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), id);
    assert_eq!(status_of(&d, &id).as_deref(), Some("stopped"));
}

#[test]
fn given_running_sandbox_when_rm_then_it_fails_and_the_sandbox_keeps_running() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    let out = run(&d, &["rm", &id]);

    // Assert
    assert!(!out.status.success());
    assert!(
        predicate::str::contains("cannot remove a running sandbox: stop it first or use -f")
            .eval(&stderr(&out)),
        "{}",
        stderr(&out)
    );
    assert_eq!(status_of(&d, &id).as_deref(), Some("running"));
}

#[test]
fn given_running_sandbox_when_rm_force_then_the_sandbox_is_gone() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    let printed = ok(&d, &["rm", "-f", &id]);

    // Assert
    assert_eq!(printed.trim(), id);
    assert_eq!(status_of(&d, &id), None);
}

#[test]
fn given_stopped_sandbox_when_rm_then_it_is_removed() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--name", "gone1"]);
    ok(&d, &["stop", "gone1"]);

    // Act
    ok(&d, &["rm", "gone1"]);

    // Assert
    assert_eq!(status_of(&d, &id), None);
}

#[test]
fn given_stopped_and_running_sandbox_when_prune_then_only_the_stopped_one_is_removed() {
    // Arrange
    let d = Daemon::spawn();
    let stopped = create(&d, &[]);
    let running = create(&d, &[]);
    ok(&d, &["stop", &stopped]);

    // Act
    let printed = ok(&d, &["prune"]);

    // Assert
    assert_eq!(printed.trim(), stopped);
    assert_eq!(status_of(&d, &stopped), None);
    assert_eq!(status_of(&d, &running).as_deref(), Some("running"));
}

#[test]
fn given_stopped_sandbox_without_stored_token_when_prune_then_it_is_skipped_and_others_removed() {
    // Arrange
    let d = Daemon::spawn();
    let orphan = create(&d, &[]);
    let other = create(&d, &[]);
    ok(&d, &["stop", &orphan, &other]);
    forget_token(&d, &orphan);

    // Act
    let out = run(&d, &["prune"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), other);
    assert!(
        stderr(&out).contains(&format!("skipped: {orphan}")),
        "{}",
        stderr(&out)
    );
    assert_eq!(status_of(&d, &orphan).as_deref(), Some("stopped"));
    assert_eq!(status_of(&d, &other), None);
}

#[test]
fn given_running_sandbox_when_ps_then_header_and_a_row_are_printed() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--name", "web1"]);

    // Act
    let printed = ok(&d, &["ps"]);

    // Assert
    let mut lines = printed.lines();
    assert_eq!(
        lines.next(),
        Some("CONTAINER ID\tIMAGE\tCOMMAND\tCREATED\tSTATUS\tNAMES")
    );
    let row: Vec<&str> = lines.next().expect("row").split('\t').collect();
    assert_eq!(row[0], &id[..12]);
    assert_eq!(row[1], "alpine:latest");
    assert!(row[4].starts_with("Up"), "{row:?}");
    assert_eq!(row[5], "web1");
    assert_eq!(lines.next(), None);
}

#[test]
fn given_running_sandbox_when_ps_format_json_then_one_object_per_line() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--name", "web1"]);
    create(&d, &["--name", "web2"]);

    // Act
    let printed = ok(&d, &["ps", "--format", "json"]);

    // Assert
    let objects: Vec<serde_json::Value> = printed
        .lines()
        .map(|l| serde_json::from_str(l).expect("one object per line"))
        .collect();
    assert_eq!(objects.len(), 2);
    let web1 = objects.iter().find(|o| o["Names"] == "web1").expect("web1");
    assert_eq!(web1["ID"], id[..12]);
    assert!(web1["CreatedAt"].as_str().unwrap().ends_with("+0000 UTC"));
    assert!(web1["RunningFor"].as_str().unwrap().ends_with("ago"));
}

#[test]
fn given_one_unknown_reference_when_inspect_then_found_entries_print_and_exit_is_nonzero() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--name", "web1"]);

    // Act
    let out = run(&d, &["inspect", "web1", "missing1"]);

    // Assert
    assert!(!out.status.success());
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).expect("json array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["Id"], id);
    assert!(stderr(&out).contains("missing1"), "{}", stderr(&out));
}

#[test]
fn given_only_unknown_references_when_inspect_then_empty_array_and_nonzero_exit() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["inspect", "missing1"]);

    // Assert
    assert!(!out.status.success());
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).expect("json array");
    assert!(entries.is_empty());
}
