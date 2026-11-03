// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward save` and `ward load` against a real wardd with
//! the stub backend and offline pulls.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::io::Write;
use std::process::{Output, Stdio};

use common::Daemon;

const IMAGE: &str = "alpine:3";
const DIGEST_FORMAT: &str = "{{.Digest}}";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn run_ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "ward {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn digest_of_images(d: &Daemon) -> String {
    run_ok(d, &["images", "--no-trunc", "--format", DIGEST_FORMAT])
        .trim()
        .to_string()
}

#[test]
fn given_a_saved_file_when_the_image_is_removed_and_loaded_then_it_returns_with_a_stable_digest() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let first = dir.path().join("first.tar");
    let second = dir.path().join("second.tar");
    run_ok(&d, &["pull", IMAGE]);

    // Act
    run_ok(&d, &["save", "-o", first.to_str().unwrap(), IMAGE]);
    run_ok(&d, &["rmi", IMAGE]);
    let loaded = run_ok(&d, &["load", "-i", first.to_str().unwrap()]);
    let digest = digest_of_images(&d);
    run_ok(&d, &["save", "-o", second.to_str().unwrap(), IMAGE]);
    run_ok(&d, &["rmi", IMAGE]);
    run_ok(&d, &["load", "-q", "-i", second.to_str().unwrap()]);

    // Assert
    assert_eq!(loaded, format!("Loaded image: {IMAGE}\n"));
    assert!(digest.starts_with("sha256:"), "{digest}");
    assert_eq!(digest_of_images(&d), digest);
    assert_eq!(
        std::fs::read(&first).expect("first"),
        std::fs::read(&second).expect("second")
    );
}

#[test]
fn given_a_saved_stream_when_piped_to_load_then_the_image_is_cached() {
    // Arrange
    let d = Daemon::spawn();
    run_ok(&d, &["pull", IMAGE]);
    let archive = run(&d, &["save", IMAGE]).stdout;
    run_ok(&d, &["rmi", IMAGE]);

    // Act
    let mut child = d
        .cli()
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn load");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&archive)
        .expect("write archive");
    let loaded = child.wait().expect("load");

    // Assert
    assert!(loaded.success(), "{loaded:?}");
    assert!(run_ok(&d, &["images"]).contains("alpine"));
}

#[test]
fn given_a_missing_image_when_saved_then_it_fails_and_no_file_is_left() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("out.tar");

    // Act
    let out = run(&d, &["save", "-o", target.to_str().unwrap(), "nope:1"]);

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("No such image: nope:1"));
    assert!(!target.exists());
}

#[test]
fn given_a_file_that_is_not_an_archive_when_loaded_then_it_fails() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let junk = dir.path().join("junk.tar");
    std::fs::write(&junk, b"not a tar file").expect("write");

    // Act
    let out = run(&d, &["load", "-i", junk.to_str().unwrap()]);

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a valid image archive"));
}
