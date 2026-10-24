// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `--exited-ttl` against a real wardd subprocess with the
//! stub backend.
//!
//! Style: BDD names with AAA bodies, one scenario per test.

mod common;

use std::time::{Duration, Instant};

use common::Daemon;

const TTL: &str = "1s";
const LONG_TTL: &str = "30s";
const GONE_DEADLINE: Duration = Duration::from_secs(20);
const POLL_STEP: Duration = Duration::from_millis(100);
const GRACE_PERIOD: Duration = Duration::from_secs(3);

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = d.cli().args(args).output().expect("run ward");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn create(d: &Daemon, extra: &[&str]) -> String {
    ok(d, &[&["create"], extra, &["alpine:latest"]].concat())
        .trim()
        .to_string()
}

fn status_of(d: &Daemon, id: &str) -> Option<String> {
    ok(d, &["list"]).lines().find_map(|l| {
        let mut cols = l.split('\t');
        (cols.next() == Some(id)).then(|| cols.next().unwrap_or_default().to_string())
    })
}

fn wait_until_gone(d: &Daemon, id: &str) -> bool {
    let deadline = Instant::now() + GONE_DEADLINE;
    while Instant::now() < deadline {
        if status_of(d, id).is_none() {
            return true;
        }
        std::thread::sleep(POLL_STEP);
    }
    false
}

#[test]
fn given_exited_ttl_when_the_sandbox_stops_then_it_disappears_after_the_ttl() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--exited-ttl", TTL]);

    // Act
    ok(&d, &["stop", &id]);

    // Assert
    assert!(
        wait_until_gone(&d, &id),
        "the stopped sandbox outlived its TTL"
    );
}

#[test]
fn given_no_exited_ttl_when_the_sandbox_stops_then_it_stays() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &[]);

    // Act
    ok(&d, &["stop", &id]);
    std::thread::sleep(GRACE_PERIOD);

    // Assert
    assert_eq!(status_of(&d, &id).as_deref(), Some("stopped"));
}

#[test]
fn given_stopped_sandbox_with_ttl_when_started_again_then_it_is_kept() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--exited-ttl", TTL]);
    ok(&d, &["stop", &id]);

    // Act
    ok(&d, &["start", &id]);
    std::thread::sleep(GRACE_PERIOD);

    // Assert
    assert_eq!(status_of(&d, &id).as_deref(), Some("running"));
}

#[test]
fn given_bad_exited_ttl_when_create_then_the_cli_rejects_it() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = d
        .cli()
        .args(["create", "--exited-ttl", "soon", "alpine:latest"])
        .output()
        .expect("run ward");

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid duration 'soon'"));
}

#[test]
fn given_long_exited_ttl_when_the_sandbox_stops_then_it_is_still_listed() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, &["--exited-ttl", LONG_TTL]);

    // Act
    ok(&d, &["stop", &id]);

    // Assert
    assert_eq!(status_of(&d, &id).as_deref(), Some("stopped"));
}
