// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for running an image's own Cmd, Entrypoint, WorkingDir and
//! Env, against a real wardd with the stub backend. The stub prints
//! `stub: <argv0>` for the main process, so the full argv is read back
//! through `ward inspect`.

#![cfg(not(feature = "krunvm"))]

mod common;

use std::process::Output;

use assert_cmd::assert::OutputAssertExt;
use common::Daemon;
use predicates::prelude::*;
use serde_json::{Value, json};

const DOCKER_CLI_FAILURE: i32 = 125;
const NO_COMMAND: &str = "no command specified";
const IMAGE: &str = "alpine";
const SHELL_IMAGE: &str = r#"{"cmd":["/bin/sh"],"working_dir":"/app","env":["A=image"]}"#;
const ENTRYPOINT_IMAGE: &str = r#"{"entrypoint":["/entry"],"cmd":["serve"]}"#;
const NO_COMMAND_IMAGE: &str = "{}";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn run_ok(d: &Daemon, args: &[&str]) {
    let out = run(d, args);
    assert!(out.status.success(), "ward {args:?} failed: {out:?}");
}

/// `Config` of the one sandbox named `name`.
fn config_of(d: &Daemon, name: &str) -> Value {
    let out = run(d, &["inspect", name]);
    assert!(out.status.success(), "inspect failed: {out:?}");
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("inspect json");
    parsed[0]["Config"].clone()
}

/// `State.Status` of the one sandbox named `name`.
fn status_of(d: &Daemon, name: &str) -> Value {
    let out = run(d, &["inspect", name]);
    assert!(out.status.success(), "inspect failed: {out:?}");
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("inspect json");
    parsed[0]["State"]["Status"].clone()
}

#[test]
fn given_image_cmd_when_run_without_command_then_the_image_cmd_starts() {
    // Arrange
    let d = Daemon::spawn_with_image_config(SHELL_IMAGE);

    // Act
    let assertion = d.cli().args(["run", IMAGE]).assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::contains("stub: /bin/sh"));
}

#[test]
fn given_image_without_command_when_run_without_command_then_it_fails_like_docker() {
    // Arrange
    let d = Daemon::spawn_with_image_config(NO_COMMAND_IMAGE);

    // Act
    let assertion = d.cli().args(["run", IMAGE]).assert();

    // Assert
    assertion
        .code(DOCKER_CLI_FAILURE)
        .stderr(predicate::str::contains(NO_COMMAND));
}

#[test]
fn given_image_with_command_when_create_without_command_then_the_sandbox_is_idle() {
    // Arrange
    let d = Daemon::spawn_with_image_config(SHELL_IMAGE);

    // Act
    let out = run(&d, &["create", "--name", "idle", IMAGE]);
    let config = config_of(&d, "idle");

    // Assert
    assert!(out.status.success(), "create failed: {out:?}");
    assert_eq!(config["Cmd"], json!([]));
    assert_eq!(status_of(&d, "idle"), "running");
}

#[test]
fn given_image_entrypoint_and_cmd_when_run_with_arguments_then_arguments_replace_the_cmd() {
    // Arrange
    let d = Daemon::spawn_with_image_config(ENTRYPOINT_IMAGE);

    // Act
    run_ok(&d, &["run", "-d", "--name", "ep", IMAGE, "--flag"]);
    let config = config_of(&d, "ep");

    // Assert
    assert_eq!(config["Entrypoint"], json!(["/entry"]));
    assert_eq!(config["Cmd"], json!(["--flag"]));
}

#[test]
fn given_image_entrypoint_and_cmd_when_run_without_arguments_then_both_apply() {
    // Arrange
    let d = Daemon::spawn_with_image_config(ENTRYPOINT_IMAGE);

    // Act
    run_ok(&d, &["run", "-d", "--name", "both", IMAGE]);
    let config = config_of(&d, "both");

    // Assert
    assert_eq!(config["Entrypoint"], json!(["/entry"]));
    assert_eq!(config["Cmd"], json!(["serve"]));
}

#[test]
fn given_entrypoint_flag_when_run_without_arguments_then_the_image_cmd_is_ignored() {
    // Arrange
    let d = Daemon::spawn_with_image_config(ENTRYPOINT_IMAGE);

    // Act
    run_ok(
        &d,
        &[
            "run",
            "-d",
            "--name",
            "over",
            "--entrypoint",
            "/other",
            IMAGE,
        ],
    );
    let config = config_of(&d, "over");

    // Assert
    assert_eq!(config["Entrypoint"], json!(["/other"]));
    assert_eq!(config["Cmd"], json!([]));
}

#[test]
fn given_empty_entrypoint_flag_when_run_with_arguments_then_only_the_arguments_run() {
    // Arrange
    let d = Daemon::spawn_with_image_config(ENTRYPOINT_IMAGE);

    // Act
    run_ok(
        &d,
        &[
            "run",
            "-d",
            "--name",
            "clear",
            "--entrypoint",
            "",
            IMAGE,
            "echo",
        ],
    );
    let config = config_of(&d, "clear");

    // Assert
    assert_eq!(config["Entrypoint"], Value::Null);
    assert_eq!(config["Cmd"], json!(["echo"]));
}

#[test]
fn given_image_working_dir_when_run_then_it_applies_unless_w_is_given() {
    // Arrange
    let d = Daemon::spawn_with_image_config(SHELL_IMAGE);

    // Act
    run_ok(&d, &["run", "-d", "--name", "dflt", IMAGE]);
    run_ok(&d, &["run", "-d", "--name", "given", "-w", "/tmp", IMAGE]);

    // Assert
    assert_eq!(config_of(&d, "dflt")["WorkingDir"], "/app");
    assert_eq!(config_of(&d, "given")["WorkingDir"], "/tmp");
}
