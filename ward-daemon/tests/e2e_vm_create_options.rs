// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Create-time flags that only a real guest can show. Needs `--features
//! krunvm` and `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_create_options

#![cfg(feature = "krunvm")]

mod common;

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
/// Characters of the sandbox ID that make the default host name.
const SHORT_ID_LEN: usize = 12;

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

const POLL_STEP: std::time::Duration = std::time::Duration::from_millis(200);
const WAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
/// Time to show the restarts have stopped at the cap.
const RESTART_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

fn wait_for_restart_count(d: &Daemon, id: &str, want: u32) {
    let deadline = std::time::Instant::now() + WAIT_DEADLINE;
    while std::time::Instant::now() < deadline {
        let count = run_ok(d, &["inspect", "--format", "{{.RestartCount}}", id]);
        if count.trim().parse::<u32>().is_ok_and(|n| n >= want) {
            return;
        }
        std::thread::sleep(POLL_STEP);
    }
    panic!("{id} never restarted {want} times");
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

#[test]
fn given_env_files_and_flag_when_run_then_the_guest_sees_the_merged_environment() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("first.env");
    let second = dir.path().join("second.env");
    std::fs::write(&first, "# first\nA=first\nB=first\nC=first\n").expect("write");
    std::fs::write(&second, "B=second\nC=second\n").expect("write");
    let image = image();

    // Act
    let out = run_ok(
        &d,
        &[
            "run",
            "--rm",
            "--env-file",
            first.to_str().unwrap(),
            "--env-file",
            second.to_str().unwrap(),
            "-e",
            "C=flag",
            &image,
            "sh",
            "-c",
            "echo $A $B $C",
        ],
    );

    // Assert
    assert_eq!(out.trim(), "first second flag");
}

#[test]
fn given_hostname_when_run_then_the_guest_reports_it() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();

    // Act
    let named = run_ok(
        &d,
        &["run", "--rm", "--hostname", "web1", &image, "hostname"],
    );
    let id = run_ok(&d, &["run", "-d", &image, "sleep", "60"]);
    let id = id.trim();
    let default = run_ok(&d, &["exec", id, "hostname"]);

    // Assert
    assert_eq!(named.trim(), "web1");
    assert_eq!(default.trim(), &id[..SHORT_ID_LEN]);
}

#[test]
fn given_stop_signal_when_stopped_then_the_guest_process_gets_it() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let id = run_ok(
        &d,
        &[
            "run",
            "-d",
            "--stop-signal",
            "SIGUSR1",
            &image,
            "sh",
            "-c",
            "trap 'exit 42' USR1; while :; do sleep 1; done",
        ],
    );
    let id = id.trim();

    // Act
    run_ok(&d, &["stop", id]);
    let code = run_ok(&d, &["inspect", "--format", "{{.State.ExitCode}}", id]);

    // Assert
    assert_eq!(code.trim(), "42");
}

#[test]
fn given_on_failure_cap_when_the_guest_command_fails_then_it_is_restarted_that_many_times() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let id = run_ok(
        &d,
        &[
            "run",
            "-d",
            "--restart",
            "on-failure:2",
            &image,
            "sh",
            "-c",
            "exit 1",
        ],
    );
    let id = id.trim();

    // Act
    wait_for_restart_count(&d, id, 2);
    std::thread::sleep(RESTART_SETTLE);
    let count = run_ok(&d, &["inspect", "--format", "{{.RestartCount}}", id]);

    // Assert
    assert_eq!(count.trim(), "2");
}
