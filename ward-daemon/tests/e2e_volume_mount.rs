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

fn volume_names(d: &Daemon) -> Vec<String> {
    ok(d, &["volume", "ls", "-q"])
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn given_selinux_and_consistency_options_when_mounted_then_only_ro_changes_the_mode() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    ok(
        &d,
        &["create", "--name", "web", "-v", "data:/data:ro,z", ALPINE],
    );
    ok(
        &d,
        &[
            "create",
            "--name",
            "store",
            "-v",
            "cache:/c:delegated",
            ALPINE,
        ],
    );

    // Assert
    let web: serde_json::Value = serde_json::from_str(&ok(&d, &["inspect", "web"])).unwrap();
    let db: serde_json::Value = serde_json::from_str(&ok(&d, &["inspect", "store"])).unwrap();
    assert_eq!(web[0]["Mounts"][0]["RW"], false);
    assert_eq!(db[0]["Mounts"][0]["RW"], true);
}

#[test]
fn given_an_unknown_mount_option_when_created_then_it_fails_with_docker_wording() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["create", "-v", "data:/data:fast", ALPINE]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("invalid mode: fast"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_an_anonymous_volume_when_the_sandbox_is_removed_then_only_rm_v_removes_it() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "kept", "-v", "/data", ALPINE]);
    ok(&d, &["create", "--name", "gone", "-v", "/data", ALPINE]);
    assert_eq!(volume_names(&d).len(), 2);

    // Act
    ok(&d, &["rm", "-f", "kept"]);
    let after_plain_rm = volume_names(&d).len();
    ok(&d, &["rm", "-fv", "gone"]);

    // Assert
    assert_eq!(after_plain_rm, 2);
    let left = volume_names(&d);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].len(), 64);
}

#[test]
fn given_a_shared_anonymous_volume_when_rm_v_then_it_goes_with_the_last_sandbox() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "owner", "-v", "/data", ALPINE]);
    ok(
        &d,
        &[
            "create",
            "--name",
            "user",
            "--volumes-from",
            "owner",
            ALPINE,
        ],
    );

    // Act
    ok(&d, &["rm", "-fv", "owner"]);
    let while_shared = volume_names(&d).len();
    ok(&d, &["rm", "-fv", "user"]);

    // Assert
    assert_eq!(while_shared, 1);
    assert!(volume_names(&d).is_empty());
}

#[test]
fn given_volumes_from_with_ro_when_created_then_the_shared_mounts_are_read_only() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "src", "-v", "data:/data", ALPINE]);

    // Act
    ok(
        &d,
        &[
            "create",
            "--name",
            "dst",
            "--volumes-from",
            "src:ro",
            ALPINE,
        ],
    );

    // Assert
    let dst: serde_json::Value = serde_json::from_str(&ok(&d, &["inspect", "dst"])).unwrap();
    let mount = &dst[0]["Mounts"][0];
    assert_eq!(mount["Name"], "data");
    assert_eq!(mount["RW"], false);
}

#[test]
fn given_volumes_from_an_unknown_sandbox_when_created_then_it_fails() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["create", "--volumes-from", "nope", ALPINE]);

    // Assert
    assert!(!out.status.success());
}
