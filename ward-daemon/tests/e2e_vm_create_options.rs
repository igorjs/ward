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
