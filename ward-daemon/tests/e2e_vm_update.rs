// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward update` against a real guest. Needs `--features krunvm` and
//! `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_update

#![cfg(feature = "krunvm")]

mod common;

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const CGROUP_DIR: &str = "/sys/fs/cgroup/ward";
const VM_MEMORY: &str = "256m";
const MIB: u64 = 1024 * 1024;

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

/// The content of a cgroup file as the guest sees it.
fn cgroup_file(d: &Daemon, name: &str, file: &str) -> String {
    let path = format!("{CGROUP_DIR}/{file}");
    run_ok(d, &["exec", name, "cat", &path]).trim().to_string()
}

#[test]
fn given_running_sandbox_when_updated_then_the_guest_cgroup_holds_the_new_limits() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &[
            "run", "-d", "--name", "box", "-m", VM_MEMORY, &image, "sleep", "600",
        ],
    );

    // Act
    run_ok(
        &d,
        &[
            "update",
            "--memory",
            "128m",
            "--cpus",
            "0.5",
            "--pids-limit",
            "50",
            "box",
        ],
    );

    // Assert
    assert_eq!(
        cgroup_file(&d, "box", "memory.max"),
        (128 * MIB).to_string()
    );
    assert_eq!(cgroup_file(&d, "box", "cpu.max"), "50000 100000");
    assert_eq!(cgroup_file(&d, "box", "pids.max"), "50");
}

#[test]
fn given_running_sandbox_when_cgroup_options_updated_then_the_guest_cgroup_holds_them() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &[
            "run", "-d", "--name", "box", "-m", VM_MEMORY, &image, "sleep", "600",
        ],
    );

    // Act
    run_ok(
        &d,
        &[
            "update",
            "--cpu-shares",
            "1024",
            "--cpu-quota",
            "50000",
            "--cpu-period",
            "100000",
            "--memory-reservation",
            "64m",
            "box",
        ],
    );

    // Assert
    assert_eq!(cgroup_file(&d, "box", "cpu.weight"), "39");
    assert_eq!(cgroup_file(&d, "box", "cpu.max"), "50000 100000");
    assert_eq!(cgroup_file(&d, "box", "memory.low"), (64 * MIB).to_string());

    // Act again: pin the CPU list and lift the quota.
    run_ok(
        &d,
        &["update", "--cpuset-cpus", "0", "--cpu-quota", "-1", "box"],
    );

    // Assert again
    assert_eq!(cgroup_file(&d, "box", "cpuset.cpus"), "0");
    assert_eq!(cgroup_file(&d, "box", "cpu.max"), "max 100000");
}

#[test]
fn given_running_sandbox_when_memory_raised_above_the_vm_then_the_update_is_refused() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &[
            "run", "-d", "--name", "box", "-m", VM_MEMORY, &image, "sleep", "600",
        ],
    );

    // Act
    let out = d
        .cli()
        .args(["update", "--memory", "1g", "box"])
        .output()
        .expect("run ward");

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("above the VM's 256 MiB"));
}

#[test]
fn given_stopped_sandbox_when_cpu_limit_set_then_it_is_applied_after_the_next_start() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    run_ok(
        &d,
        &[
            "run", "-d", "--name", "box", "--cpus", "2", &image, "sleep", "600",
        ],
    );
    run_ok(&d, &["stop", "-t", "2", "box"]);
    run_ok(&d, &["update", "--cpus", "0.5", "box"]);

    // Act
    run_ok(&d, &["start", "box"]);

    // Assert
    assert_eq!(cgroup_file(&d, "box", "cpu.max"), "50000 100000");
}
