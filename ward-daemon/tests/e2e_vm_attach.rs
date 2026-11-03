// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward attach`, `start -a` and `run -a` against a real guest. Needs
//! `--features krunvm` and `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the
//! image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_attach

#![cfg(feature = "krunvm")]

mod common;

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const BOOT_WAIT: Duration = Duration::from_secs(2);
const EXIT_CODE: i32 = 7;
const CTRL_P: u8 = 0x10;
const CTRL_Q: u8 = 0x11;

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

#[test]
fn given_main_process_reading_stdin_when_attached_then_input_is_echoed_and_exit_code_is_its_own() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let script = format!("read line; echo got:$line; exit {EXIT_CODE}");
    run_ok(
        &d,
        &[
            "run", "-d", "--name", "reader", "-i", &image, "sh", "-c", &script,
        ],
    );
    std::thread::sleep(BOOT_WAIT);

    // Act
    let mut child = d
        .cli()
        .args(["attach", "reader"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attach");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");

    // Assert
    assert_eq!(out.status.code(), Some(EXIT_CODE));
    assert!(String::from_utf8_lossy(&out.stdout).contains("got:hello"));
}

#[test]
fn given_attached_sandbox_when_detach_keys_are_typed_then_the_cli_exits_and_the_process_keeps_running()
 {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &["run", "-d", "--name", "idle", "-i", &image, "sleep", "600"],
    );
    std::thread::sleep(BOOT_WAIT);
    let mut child = d
        .cli()
        .args(["attach", "idle"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attach");

    // Act
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(&[CTRL_P, CTRL_Q])
        .expect("write");
    let out = child.wait_with_output().expect("wait");

    // Assert
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("read escape sequence"));
    let state = run_ok(&d, &["inspect", "--format", "{{.State.Status}}", "idle"]);
    assert_eq!(state.trim(), "running");
}

#[test]
fn given_exited_sandbox_when_started_with_a_then_the_exit_code_propagates() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let script = format!("echo started; exit {EXIT_CODE}");
    let _ = d
        .cli()
        .args(["run", "--name", "job", &image, "sh", "-c", &script])
        .output()
        .expect("run");

    // Act
    let out = d
        .cli()
        .args(["start", "-a", "job"])
        .output()
        .expect("start");

    // Assert
    assert_eq!(out.status.code(), Some(EXIT_CODE));
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));
}

#[test]
fn given_command_writing_to_both_streams_when_run_a_stdout_then_stderr_is_hidden() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();

    // Act
    let out = d
        .cli()
        .args([
            "run",
            "--rm",
            "-a",
            "stdout",
            &image,
            "sh",
            "-c",
            "echo out; echo err >&2",
        ])
        .output()
        .expect("run");

    // Assert
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "out");
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "");
}
