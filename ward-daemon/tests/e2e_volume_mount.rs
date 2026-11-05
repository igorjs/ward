// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for named volumes mounted into sandboxes. They run the real
//! `ward` binary against a stub-backend `wardd`.

mod common;

use std::process::Output;

use common::Daemon;

const ALPINE: &str = "alpine:latest";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    stdout(&out)
}

#[test]
fn given_a_missing_volume_when_a_sandbox_mounts_it_then_the_volume_is_created() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    ok(&d, &["create", "--name", "web", "-v", "data:/data", ALPINE]);

    // Assert
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "data\n");
}

#[test]
fn given_a_mount_flag_of_type_volume_when_created_then_the_volume_exists() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    ok(
        &d,
        &["create", "--mount", "type=volume,src=cache,dst=/c", ALPINE],
    );

    // Assert
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "cache\n");
}

#[test]
fn given_a_volume_in_use_when_removed_then_it_fails_until_the_sandbox_is_gone() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web", "-v", "data:/data", ALPINE]);

    // Act
    let busy = run(&d, &["volume", "rm", "data"]);
    ok(&d, &["rm", "-f", "web"]);
    let freed = run(&d, &["volume", "rm", "data"]);

    // Assert
    assert!(!busy.status.success());
    assert!(
        stderr(&busy).contains("volume is in use"),
        "{}",
        stderr(&busy)
    );
    assert!(freed.status.success(), "{}", stderr(&freed));
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "");
}

#[test]
fn given_a_volume_in_use_when_pruned_then_only_the_unused_one_goes() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "-v", "used:/data", ALPINE]);
    ok(&d, &["volume", "create", "idle", "--size", "10"]);

    // Act
    let report = ok(&d, &["volume", "prune", "-f"]);

    // Assert
    assert!(report.contains("idle"), "{report}");
    assert!(!report.contains("used"), "{report}");
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "used\n");
}

#[test]
fn given_a_volume_in_use_when_listed_dangling_then_only_the_idle_one_shows() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "-v", "used:/data", ALPINE]);
    ok(&d, &["volume", "create", "idle", "--size", "10"]);

    // Act
    let dangling = ok(&d, &["volume", "ls", "-q", "--filter", "dangling=true"]);

    // Assert
    assert_eq!(dangling, "idle\n");
}

#[test]
fn given_a_volume_mount_when_ps_and_inspect_then_they_show_the_volume() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &["create", "--name", "web", "-v", "data:/data:ro", ALPINE],
    );

    // Act
    let mounts = ok(&d, &["ps", "-a", "--format", "{{.Mounts}}"]);
    let inspected: serde_json::Value =
        serde_json::from_str(&ok(&d, &["inspect", "web"])).expect("json");

    // Assert
    assert_eq!(mounts, "data\n");
    let mount = &inspected[0]["Mounts"][0];
    assert_eq!(mount["Type"], "volume");
    assert_eq!(mount["Name"], "data");
    assert_eq!(mount["Destination"], "/data");
    assert_eq!(mount["RW"], false);
}

#[test]
fn given_an_invalid_volume_name_when_mounted_then_the_create_fails() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["create", "-v", "-bad:/data", ALPINE]);

    // Assert
    assert!(!out.status.success());
}

#[test]
fn given_a_volume_in_use_when_system_df_verbose_then_links_count_the_sandbox() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "-v", "data:/data", ALPINE]);

    // Act
    let report = ok(&d, &["system", "df", "-v"]);

    // Assert
    let row = report
        .lines()
        .find(|l| l.starts_with("data "))
        .expect("volume row");
    assert!(row.split_whitespace().nth(1) == Some("1"), "{row}");
}
