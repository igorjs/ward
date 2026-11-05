// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Named volumes in a real guest. Needs `--features krunvm` and
//! `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_volume

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

/// Run `script` in a throwaway sandbox with `flags` and return its output.
fn run_script(d: &Daemon, flags: &[&str], script: &str) -> String {
    let image = image();
    let mut args = vec!["run", "--rm"];
    args.extend(flags);
    args.extend([image.as_str(), "sh", "-c", script]);
    run_ok(d, &args)
}

#[test]
fn given_a_named_volume_when_two_sandboxes_run_then_the_second_reads_the_first_write() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();

    // Act
    run_script(&d, &["-v", "keep:/data"], "echo persisted > /data/note");
    let second = run_script(&d, &["-v", "keep:/data"], "cat /data/note");

    // Assert
    assert_eq!(second.trim(), "persisted");
    assert_eq!(run_ok(&d, &["volume", "ls", "-q"]), "keep\n");
}

#[test]
fn given_a_read_only_volume_mount_when_written_then_the_write_fails() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let seeded = run_script(
        &d,
        &["-v", "ro-data:/data"],
        "echo x > /data/f && echo seeded",
    );
    assert_eq!(seeded.trim(), "seeded");

    // Act
    let out = run_script(
        &d,
        &["--mount", "type=volume,src=ro-data,dst=/data,ro"],
        "cat /data/f; touch /data/g 2>&1 || echo write-refused",
    );

    // Assert
    assert!(out.contains('x'), "{out}");
    assert!(out.contains("write-refused"), "{out}");
}
