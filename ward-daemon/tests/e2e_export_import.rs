// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward export`, `import` and `pull --platform` against a
//! real wardd with the stub backend and offline pulls.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::process::Output;

use common::Daemon;

const IMAGE: &str = "alpine:3";
const IMPORTED: &str = "imported:v1";
const IMAGE_ID_PREFIX: &str = "sha256:";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn run_ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "ward {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn entries(tar_path: &std::path::Path) -> Vec<String> {
    let mut archive = tar::Archive::new(std::fs::File::open(tar_path).expect("open tar"));
    archive
        .entries()
        .expect("entries")
        .map(|e| {
            e.expect("entry")
                .path()
                .expect("path")
                .display()
                .to_string()
        })
        .collect()
}

#[test]
fn given_a_sandbox_when_exported_then_the_tar_imports_as_a_tagged_image() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let tar_path = dir.path().join("fs.tar");
    let tar_arg = tar_path.to_str().unwrap();
    let sandbox = run_ok(&d, &["create", IMAGE]);
    run_ok(&d, &["stop", sandbox.trim()]);

    // Act
    run_ok(&d, &["export", "-o", tar_arg, sandbox.trim()]);
    let id = run_ok(&d, &["import", "-m", "from export", tar_arg, IMPORTED]);
    let listed = run_ok(&d, &["images"]);
    let history = run_ok(&d, &["history", IMPORTED]);

    // Assert
    assert!(entries(&tar_path).iter().any(|e| e.starts_with("bin")));
    assert!(id.starts_with(IMAGE_ID_PREFIX), "{id}");
    assert!(listed.contains("imported"), "{listed}");
    assert!(history.contains("from export"), "{history}");
}

#[test]
fn given_a_missing_sandbox_when_exported_then_it_fails() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["export", "-o", "/dev/null", "ghost"]);

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No such container"));
}

#[test]
fn given_a_file_that_is_not_a_tar_or_a_url_when_imported_then_it_fails() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let junk = dir.path().join("junk");
    std::fs::write(&junk, b"plain text, not a tar archive at all").expect("write");

    // Act
    let not_tar = run(&d, &["import", junk.to_str().unwrap()]);
    let url = run(&d, &["import", "https://example.com/fs.tar"]);

    // Assert
    assert!(!not_tar.status.success());
    assert!(String::from_utf8_lossy(&not_tar.stderr).contains("not a tar archive"));
    assert!(String::from_utf8_lossy(&url.stderr).contains("URL is not supported"));
    assert!(run_ok(&d, &["images", "-q"]).is_empty());
}

#[test]
fn given_an_unsupported_platform_or_all_tags_when_pulled_then_the_message_says_why() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let platform = run(&d, &["pull", "--platform", "windows/amd64", IMAGE]);
    let all_tags = run(&d, &["pull", "--all-tags", "alpine"]);

    // Assert
    assert!(String::from_utf8_lossy(&platform.stderr).contains("not supported"));
    assert!(String::from_utf8_lossy(&all_tags.stderr).contains("--all-tags is not supported"));
    assert!(run_ok(&d, &["images", "-q"]).is_empty());
}
