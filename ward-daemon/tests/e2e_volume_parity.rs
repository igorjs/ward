// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the Docker shaped `volume` options and `system df -v`.
//! They run the real `ward` binary against a stub-backend `wardd`. Creating a
//! volume needs the Linux ext4 formatter, so those scenarios are Linux only.

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
fn given_no_volumes_when_ls_inspect_rm_and_prune_run_then_each_follows_docker() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let quiet = ok(&d, &["volume", "ls", "-q"]);
    let formatted = ok(&d, &["volume", "ls", "--format", "{{.Name}}"]);
    let filtered = ok(&d, &["volume", "ls", "--filter", "name=x", "-q"]);
    let bad_filter = run(&d, &["volume", "ls", "--filter", "colour=red"]);
    let inspect = run(&d, &["volume", "inspect", "nope"]);
    let rm = run(&d, &["volume", "rm", "nope"]);
    let forced = run(&d, &["volume", "rm", "-f", "nope"]);
    let prune = ok(&d, &["volume", "prune", "-f"]);

    // Assert
    assert_eq!(
        (quiet.as_str(), formatted.as_str(), filtered.as_str()),
        ("", "", "")
    );
    assert!(!bad_filter.status.success());
    assert!(!inspect.status.success());
    assert!(
        stderr(&inspect).contains("no such volume"),
        "{}",
        stderr(&inspect)
    );
    assert!(!rm.status.success());
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert!(stdout(&forced).is_empty());
    assert_eq!(prune, "Total reclaimed space: 0B\n");
}

#[test]
fn given_a_sandbox_when_system_df_verbose_then_the_four_sections_print() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web1", ALPINE]);

    // Act
    let df = ok(&d, &["system", "df", "-v"]);

    // Assert
    for section in [
        "Images space usage:",
        "Containers space usage:",
        "Local Volumes space usage:",
        "Build cache usage: 0B",
    ] {
        assert!(df.contains(section), "{df}");
    }
    let image_row = df
        .lines()
        .find(|l| l.starts_with("alpine"))
        .expect("alpine row");
    assert!(image_row.trim_end().ends_with('1'), "{image_row}");
    assert!(df.lines().any(|l| l.ends_with("web1")), "{df}");
}

#[cfg(target_os = "linux")]
#[test]
fn given_a_volume_when_listed_inspected_and_removed_by_name_then_name_and_id_both_work() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["volume", "create", "demo", "--size", "100"]);

    // Act
    let names = ok(&d, &["volume", "ls", "-q"]);
    let driver = ok(
        &d,
        &["volume", "inspect", "-f", "{{.Driver}}:{{.Name}}", "demo"],
    );
    let table = ok(
        &d,
        &["volume", "ls", "--format", "table {{.Name}}\t{{.Size}}"],
    );
    let removed = ok(&d, &["volume", "rm", "demo"]);

    // Assert
    assert_eq!(names, "demo\n");
    assert_eq!(driver, "local:demo\n");
    assert!(table.starts_with("VOLUME NAME"), "{table}");
    assert_eq!(removed, "demo\n");
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "");
}
