// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward tag`, `rmi` and `history` against a real wardd
//! with the stub backend and offline pulls.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::process::Output;

use common::Daemon;

const IMAGE: &str = "alpine:3";
const ALIAS: &str = "mine:v1";
const IMAGE_CONFIG: &str = r#"{"history":[
    {"created":"2024-01-02T03:04:05Z","created_by":"ADD rootfs","size_bytes":7800000},
    {"created_by":"CMD [\"sh\"]","empty_layer":true}]}"#;

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn run_ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "ward {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn given_a_tagged_image_when_one_tag_is_removed_then_the_image_stays() {
    // Arrange
    let d = Daemon::spawn();
    run_ok(&d, &["pull", IMAGE]);
    run_ok(&d, &["tag", IMAGE, ALIAS]);

    // Act
    let removed = run_ok(&d, &["rmi", ALIAS]);
    let listed = run_ok(&d, &["images"]);

    // Assert
    assert_eq!(removed, format!("Untagged: {ALIAS}\n"));
    assert!(listed.contains("alpine"), "{listed}");
    assert!(!listed.contains("mine"), "{listed}");
}

#[test]
fn given_two_tags_when_rmi_all_then_every_image_goes_and_the_exit_is_zero() {
    // Arrange
    let d = Daemon::spawn();
    run_ok(&d, &["pull", IMAGE]);
    run_ok(&d, &["tag", IMAGE, ALIAS]);

    // Act
    let removed = run_ok(&d, &["rmi", "-a"]);
    let listed = run_ok(&d, &["images", "-q"]);

    // Assert
    assert!(removed.contains(&format!("Untagged: {ALIAS}")), "{removed}");
    assert!(removed.contains("Deleted: sha256:"), "{removed}");
    assert!(listed.is_empty(), "{listed}");
}

#[test]
fn given_an_empty_cache_when_rmi_all_then_nothing_prints_and_the_exit_is_zero() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let removed = run_ok(&d, &["rmi", "--all"]);

    // Assert
    assert_eq!(removed, "");
}

#[test]
fn given_the_last_tag_when_removed_then_the_image_is_deleted() {
    // Arrange
    let d = Daemon::spawn();
    run_ok(&d, &["pull", IMAGE]);

    // Act
    let removed = run_ok(&d, &["rmi", IMAGE]);
    let listed = run_ok(&d, &["images", "-q"]);

    // Assert
    assert!(
        removed.starts_with("Untagged: alpine:3\nDeleted: sha256:"),
        "{removed}"
    );
    assert!(listed.is_empty(), "{listed}");
}

#[test]
fn given_a_missing_image_when_removed_then_the_others_still_go_and_the_exit_is_nonzero() {
    // Arrange
    let d = Daemon::spawn();
    run_ok(&d, &["pull", IMAGE]);

    // Act
    let out = run(&d, &["rmi", "nope:1", IMAGE]);

    // Assert
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("Untagged: alpine:3"), "{stdout}");
    assert!(stderr.contains("No such image: nope:1"), "{stderr}");
    assert!(
        stderr.contains("failed to remove images: [nope:1]"),
        "{stderr}"
    );
}

#[test]
fn given_a_sandbox_using_the_image_when_removed_then_stopped_needs_force_and_running_is_refused() {
    // Arrange
    let d = Daemon::spawn();
    let sandbox = run_ok(&d, &["create", IMAGE]);
    let running = run(&d, &["rmi", "-f", IMAGE]);
    run_ok(&d, &["stop", sandbox.trim()]);

    // Act
    let refused = run(&d, &["rmi", IMAGE]);
    let forced = run_ok(&d, &["rmi", "-f", IMAGE]);

    // Assert
    let stderr = String::from_utf8_lossy(&running.stderr);
    assert!(stderr.contains("cannot be forced"), "{stderr}");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("must be forced"), "{stderr}");
    assert!(forced.contains("Deleted: sha256:"), "{forced}");
}

#[test]
fn given_an_image_config_when_history_is_read_then_layers_print_newest_first() {
    // Arrange
    let d = Daemon::spawn_with_image_config(IMAGE_CONFIG);
    run_ok(&d, &["pull", IMAGE]);

    // Act
    let table = run_ok(&d, &["history", IMAGE]);
    let json = run_ok(&d, &["history", "--format", "json", "--no-trunc", IMAGE]);
    let ids = run_ok(&d, &["history", "-q", IMAGE]);

    // Assert
    let rows: Vec<&str> = table.lines().collect();
    assert!(rows[0].starts_with("IMAGE"), "{table}");
    assert!(rows[1].contains("CMD"), "{table}");
    assert!(rows[2].starts_with("<missing>"), "{table}");
    assert!(rows[2].contains("7.8MB"), "{table}");
    assert_eq!(json.lines().count(), 2, "{json}");
    assert_eq!(ids.lines().last(), Some("<missing>"));
}

#[test]
fn given_human_false_when_history_is_read_then_sizes_are_bytes() {
    // Arrange
    let d = Daemon::spawn_with_image_config(IMAGE_CONFIG);
    run_ok(&d, &["pull", IMAGE]);

    // Act
    let table = run_ok(&d, &["history", "--human=false", IMAGE]);

    // Assert
    assert!(table.contains("7800000"), "{table}");
    assert!(!table.contains("7.8MB"), "{table}");
}

#[test]
fn given_a_missing_source_when_tagged_then_it_fails() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["tag", "nope:1", ALIAS]);

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No such image: nope:1"));
}
