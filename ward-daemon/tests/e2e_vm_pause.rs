// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward pause` against a real guest. Needs `--features krunvm` and
//! `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_pause

#![cfg(feature = "krunvm")]

mod common;

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
/// One number per tick, so the log line count is the counter.
const COUNTER_SCRIPT: &str = "i=0; while true; do i=$((i+1)); echo $i; sleep 0.1; done";
const RUN_SPAN: std::time::Duration = std::time::Duration::from_secs(2);
const SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

macro_rules! require_vm {
    () => {
        if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
            eprintln!("skipped: set {ENABLE_ENV}=1 to run real-VM tests");
            return;
        }
    };
}

fn image() -> String {
    std::env::var(IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn run_ok(d: &Daemon, args: &[&str]) -> String {
    let out = d.cli().args(args).output().expect("run ward");
    assert!(
        out.status.success(),
        "{args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn counter(d: &Daemon, name: &str) -> usize {
    run_ok(d, &["logs", name]).lines().count()
}

#[test]
fn given_counting_sandbox_when_paused_then_counter_stops_and_resumes_after_unpause() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &[
            "run",
            "-d",
            "--name",
            "ticker",
            &image,
            "sh",
            "-c",
            COUNTER_SCRIPT,
        ],
    );
    std::thread::sleep(RUN_SPAN);
    assert!(counter(&d, "ticker") > 0, "the counter never started");

    // Act
    run_ok(&d, &["pause", "ticker"]);
    std::thread::sleep(SETTLE);
    let frozen_at = counter(&d, "ticker");
    std::thread::sleep(RUN_SPAN);
    let still_frozen = counter(&d, "ticker");
    run_ok(&d, &["unpause", "ticker"]);
    std::thread::sleep(RUN_SPAN);
    let resumed = counter(&d, "ticker");

    // Assert
    assert_eq!(still_frozen, frozen_at, "the counter advanced while paused");
    assert!(resumed > frozen_at, "the counter did not resume");
}

#[test]
fn given_paused_sandbox_when_stopped_then_the_guest_process_ends() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(&d, &["run", "-d", "--name", "idle", &image, "sleep", "600"]);
    run_ok(&d, &["pause", "idle"]);

    // Act
    run_ok(&d, &["stop", "-t", "2", "idle"]);

    // Assert
    let state = run_ok(&d, &["inspect", "--format", "{{.State.Status}}", "idle"]);
    assert_eq!(state.trim(), "stopped");
}
