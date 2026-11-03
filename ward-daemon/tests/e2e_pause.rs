// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward pause` and `ward unpause` against a real wardd
//! subprocess with the stub backend.

mod common;

use std::process::Output;

use common::Daemon;

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

fn create(d: &Daemon, name: &str) {
    ok(d, &["create", "--name", name, "alpine:latest"]);
}

fn status_of(d: &Daemon, name: &str) -> String {
    ok(
        d,
        &[
            "ps",
            "-a",
            "--filter",
            &format!("name={name}"),
            "--format",
            "{{.Status}}",
        ],
    )
}

#[test]
fn given_running_sandbox_when_paused_then_ps_shows_up_paused_and_inspect_says_paused() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");

    // Act
    let printed = ok(&d, &["pause", "web1"]);

    // Assert
    assert_eq!(printed, "web1");
    assert!(
        status_of(&d, "web1").ends_with("(Paused)"),
        "{}",
        status_of(&d, "web1")
    );
    assert_eq!(
        ok(&d, &["inspect", "--format", "{{.State.Paused}}", "web1"]),
        "true"
    );
    assert_eq!(
        ok(&d, &["inspect", "--format", "{{.State.Status}}", "web1"]),
        "paused"
    );
    assert_eq!(
        ok(&d, &["ps", "--filter", "status=paused", "-q"])
            .lines()
            .count(),
        1
    );
}

#[test]
fn given_paused_sandbox_when_unpaused_then_status_has_no_paused_suffix() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    ok(&d, &["pause", "web1"]);

    // Act
    let printed = ok(&d, &["unpause", "web1"]);

    // Assert
    assert_eq!(printed, "web1");
    assert!(!status_of(&d, "web1").contains("Paused"));
}

#[test]
fn given_paused_sandbox_when_exec_then_it_fails_saying_paused() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    ok(&d, &["pause", "web1"]);

    // Act
    let out = run(&d, &["exec", "web1", "true"]);

    // Assert
    assert!(!out.status.success());
    assert!(stderr(&out).contains("is paused"), "{}", stderr(&out));
}

#[test]
fn given_paused_sandbox_when_stopped_then_it_stops() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    ok(&d, &["pause", "web1"]);

    // Act
    ok(&d, &["stop", "web1"]);

    // Assert
    assert_eq!(
        ok(&d, &["inspect", "--format", "{{.State.Paused}}", "web1"]),
        "false"
    );
    assert_eq!(
        ok(&d, &["inspect", "--format", "{{.State.Status}}", "web1"]),
        "stopped"
    );
}

#[test]
fn given_paused_sandbox_when_force_removed_then_it_is_gone() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    ok(&d, &["pause", "web1"]);

    // Act
    ok(&d, &["rm", "-f", "web1"]);

    // Assert
    assert_eq!(ok(&d, &["ps", "-a", "-q"]), "");
}

#[test]
fn given_running_sandbox_when_unpaused_then_it_fails_saying_not_paused() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");

    // Act
    let out = run(&d, &["unpause", "web1"]);

    // Assert
    assert!(!out.status.success());
    assert!(stderr(&out).contains("is not paused"), "{}", stderr(&out));
}

#[test]
fn given_two_sandboxes_when_container_pause_with_one_unknown_then_the_known_one_pauses_and_exit_is_nonzero()
 {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");

    // Act
    let out = run(&d, &["container", "pause", "web1", "ghost"]);

    // Assert
    assert!(!out.status.success());
    assert_eq!(stdout(&out), "web1");
    assert!(status_of(&d, "web1").ends_with("(Paused)"));
}
