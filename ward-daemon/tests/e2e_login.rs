// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward login`, `logout` and the `image` group aliases
//! against a real wardd with the stub backend and offline pulls.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Output, Stdio};

use common::Daemon;

const SERVER: &str = "registry.example.com:5000";
const SECRET: &str = "hunter2-secret";
const OWNER_ONLY: u32 = 0o600;

fn login(d: &Daemon, config: &std::path::Path, args: &[&str], stdin: &str) -> Output {
    let mut child = d
        .cli()
        .env("DOCKER_CONFIG", config)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ward");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

#[test]
fn given_credentials_when_logged_in_then_config_json_holds_them_privately_and_logout_removes_them()
{
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("config.json");

    // Act
    let logged_in = login(
        &d,
        dir.path(),
        &["login", "-u", "ada", "--password-stdin", SERVER],
        &format!("{SECRET}\n"),
    );
    let saved = std::fs::read_to_string(&file).expect("config written");
    let logged_out = login(&d, dir.path(), &["logout", SERVER], "");
    let again = login(&d, dir.path(), &["logout", SERVER], "");

    // Assert
    assert!(logged_in.status.success(), "{logged_in:?}");
    assert_eq!(
        String::from_utf8_lossy(&logged_in.stdout),
        "Login Succeeded\n"
    );
    assert!(saved.contains(SERVER), "{saved}");
    assert!(
        !saved.contains(SECRET),
        "password stored in the clear: {saved}"
    );
    assert!(!String::from_utf8_lossy(&logged_in.stderr).contains(SECRET));
    let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, OWNER_ONLY);
    assert!(String::from_utf8_lossy(&logged_out.stdout).contains("Removing login credentials"));
    assert!(String::from_utf8_lossy(&again.stdout).contains("Not logged in"));
}

#[test]
fn given_no_password_source_when_logging_in_then_it_fails_without_writing() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");

    // Act
    let out = login(&d, dir.path(), &["login", "-u", "ada", SERVER], "");

    // Assert
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--password-stdin"));
    assert!(!dir.path().join("config.json").exists());
}

#[test]
fn given_the_image_group_when_rm_and_tag_are_used_then_they_act_like_the_top_level_commands() {
    // Arrange
    let d = Daemon::spawn();
    let run = |args: &[&str]| d.cli().args(args).output().expect("run ward");
    assert!(run(&["image", "pull", "alpine:3"]).status.success());

    // Act
    let tagged = run(&["image", "tag", "alpine:3", "mine:v1"]);
    let removed = run(&["image", "rm", "mine:v1"]);

    // Assert
    assert!(tagged.status.success());
    assert_eq!(
        String::from_utf8_lossy(&removed.stdout),
        "Untagged: mine:v1\n"
    );
}
