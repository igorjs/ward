// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the Docker shaped `volume` options and `system df -v`.
//! They run the real `ward` binary against a stub-backend `wardd`.

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
fn given_no_volumes_when_ls_quiet_then_nothing_prints() {
    let d = Daemon::spawn();

    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "");
}

#[test]
fn given_no_volumes_when_ls_formatted_then_nothing_prints() {
    let d = Daemon::spawn();

    assert_eq!(ok(&d, &["volume", "ls", "--format", "{{.Name}}"]), "");
}

#[test]
fn given_no_volumes_when_ls_filtered_then_nothing_prints() {
    let d = Daemon::spawn();

    assert_eq!(ok(&d, &["volume", "ls", "--filter", "name=x", "-q"]), "");
}

#[test]
fn given_an_unknown_filter_when_ls_then_it_fails() {
    let d = Daemon::spawn();

    let out = run(&d, &["volume", "ls", "--filter", "colour=red"]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("colour"), "{}", stderr(&out));
}

#[test]
fn given_a_missing_volume_when_inspected_then_it_prints_an_empty_array_and_one_error() {
    let d = Daemon::spawn();

    let out = run(&d, &["volume", "inspect", "nope"]);

    assert!(!out.status.success());
    assert_eq!(stdout(&out).trim(), "[]");
    assert_eq!(
        stderr(&out).trim(),
        "Error response from daemon: get nope: no such volume"
    );
}

#[test]
fn given_a_missing_volume_when_removed_then_it_fails_with_docker_wording() {
    let d = Daemon::spawn();

    let out = run(&d, &["volume", "rm", "nope"]);

    assert!(!out.status.success());
    assert_eq!(
        stderr(&out).trim(),
        "Error response from daemon: get nope: no such volume"
    );
}

#[test]
fn given_a_missing_volume_when_force_removed_then_it_is_skipped_silently() {
    let d = Daemon::spawn();

    let out = run(&d, &["volume", "rm", "-f", "nope"]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
    assert!(stderr(&out).is_empty());
}

#[test]
fn given_no_volumes_when_pruned_then_only_the_total_prints() {
    let d = Daemon::spawn();

    assert_eq!(
        ok(&d, &["volume", "prune", "-f"]),
        "Total reclaimed space: 0B\n"
    );
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
        .find(|l| l.split_whitespace().next() == Some("alpine"))
        .expect("alpine row");
    let cells: Vec<&str> = image_row.split_whitespace().collect();
    assert_eq!(cells.last().copied(), Some("1"), "{image_row}");
    assert!(df.lines().any(|l| l.ends_with("web1")), "{df}");
}

#[test]
fn given_a_sandbox_made_from_an_untagged_name_when_system_df_verbose_then_the_image_counts_it() {
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web1", "alpine"]);

    let df = ok(&d, &["system", "df", "-v"]);

    let image_row = df
        .lines()
        .find(|l| l.split_whitespace().next() == Some("alpine"))
        .expect("alpine row");
    assert_eq!(
        image_row.split_whitespace().last(),
        Some("1"),
        "{image_row}"
    );
}

#[test]
fn given_json_when_system_df_verbose_then_it_is_refused() {
    let d = Daemon::spawn();

    let out = run(&d, &["--json", "system", "df", "-v"]);

    assert!(!out.status.success());
    assert!(stderr(&out).contains("--json"), "{}", stderr(&out));
}

fn create_volumes(d: &Daemon, names: &[&str]) {
    for name in names {
        ok(d, &["volume", "create", name, "--size", "100"]);
    }
}

#[test]
fn given_a_volume_when_listed_inspected_and_removed_by_name_then_name_and_id_both_work() {
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["demo"]);

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

#[test]
fn given_several_volumes_and_one_missing_when_removed_then_the_rest_go_and_it_exits_1() {
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["a", "b"]);

    // Act
    let out = run(&d, &["volume", "rm", "a", "nope", "b"]);

    // Assert
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "a\nb\n");
    assert_eq!(
        stderr(&out).trim(),
        "Error response from daemon: get nope: no such volume"
    );
    assert_eq!(ok(&d, &["volume", "ls", "-q"]), "");
}

#[test]
fn given_a_real_and_a_missing_volume_when_force_removed_then_the_real_one_goes_and_it_succeeds() {
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["real"]);

    // Act
    let out = run(&d, &["volume", "rm", "-f", "nope", "real"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "real\n");
    assert!(stderr(&out).is_empty(), "{}", stderr(&out));
}

#[test]
fn given_the_same_volume_twice_when_removed_then_the_repeat_is_an_error_but_skipped_with_force() {
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["dup", "dup2"]);

    // Act
    let plain = run(&d, &["volume", "rm", "dup", "dup"]);
    let forced = run(&d, &["volume", "rm", "-f", "dup2", "dup2"]);

    // Assert
    assert_eq!(plain.status.code(), Some(1));
    assert_eq!(stdout(&plain), "dup\n");
    assert_eq!(
        stderr(&plain).trim(),
        "Error response from daemon: get dup: no such volume"
    );
    assert!(forced.status.success(), "{}", stderr(&forced));
    assert_eq!(stdout(&forced), "dup2\n");
}

#[test]
fn given_one_found_and_one_missing_volume_when_inspected_then_the_found_one_prints_and_it_exits_1()
{
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["here"]);

    // Act
    let out = run(&d, &["volume", "inspect", "here", "gone"]);

    // Assert
    assert_eq!(out.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json array");
    assert_eq!(parsed.as_array().map(Vec::len), Some(1));
    assert_eq!(parsed[0]["Name"], "here");
    assert_eq!(
        stderr(&out).trim(),
        "Error response from daemon: get gone: no such volume"
    );
}

#[test]
fn given_repeated_driver_filters_when_listed_then_either_value_matches() {
    // Arrange
    let d = Daemon::spawn();
    create_volumes(&d, &["one"]);

    // Act
    let out = ok(
        &d,
        &[
            "volume",
            "ls",
            "-q",
            "--filter",
            "driver=nfs",
            "--filter",
            "driver=local",
        ],
    );

    // Assert
    assert_eq!(out, "one\n");
}
