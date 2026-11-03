// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward update` against a real wardd subprocess with the
//! stub backend.

mod common;

use std::process::Output;

use common::Daemon;

const MIB: u64 = 1024 * 1024;

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    stdout(&out)
}

fn create(d: &Daemon) {
    ok(
        d,
        &[
            "create",
            "--name",
            "web1",
            "-m",
            "256m",
            "--cpus",
            "2",
            "alpine:latest",
        ],
    );
}

fn host_config(d: &Daemon, field: &str) -> String {
    ok(
        d,
        &[
            "inspect",
            "--format",
            &format!("{{{{.HostConfig.{field}}}}}"),
            "web1",
        ],
    )
}

#[test]
fn given_running_sandbox_when_updated_then_inspect_shows_the_new_limits() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);

    // Act
    let printed = ok(
        &d,
        &[
            "update",
            "--memory",
            "128m",
            "--cpus",
            "0.5",
            "--pids-limit",
            "64",
            "--restart",
            "on-failure:2",
            "web1",
        ],
    );

    // Assert
    assert_eq!(printed, "web1");
    assert_eq!(host_config(&d, "Memory"), (128 * MIB).to_string());
    assert_eq!(host_config(&d, "NanoCpus"), "500000000");
    assert_eq!(host_config(&d, "PidsLimit"), "64");
    assert_eq!(host_config(&d, "RestartPolicy.Name"), "on-failure");
    assert_eq!(host_config(&d, "RestartPolicy.MaximumRetryCount"), "2");
}

#[test]
fn given_no_flags_when_updated_then_it_fails_asking_for_one() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);

    // Act
    let out = run(&d, &["update", "web1"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("one or more flags"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_running_sandbox_when_memory_raised_above_the_vm_then_it_fails_and_keeps_the_limit() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);

    // Act
    let out = run(&d, &["update", "-m", "1g", "web1"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("above the VM's 256 MiB"),
        "{}",
        stderr(&out)
    );
    assert_eq!(host_config(&d, "Memory"), (256 * MIB).to_string());
}

#[test]
fn given_stopped_sandbox_when_memory_raised_then_it_is_accepted() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);
    ok(&d, &["stop", "web1"]);

    // Act
    ok(&d, &["container", "update", "-m", "1g", "web1"]);

    // Assert
    assert_eq!(host_config(&d, "Memory"), (1024 * MIB).to_string());
}

#[test]
fn given_two_references_when_one_is_unknown_then_the_known_one_updates_and_exit_is_nonzero() {
    // Arrange
    let d = Daemon::spawn();
    create(&d);

    // Act
    let out = run(&d, &["update", "--pids-limit", "10", "web1", "ghost"]);

    // Assert
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "web1");
    assert_eq!(host_config(&d, "PidsLimit"), "10");
}
