// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the Podman extras: `exists`, `ps --sort` and `--latest`.
//! They run the real `ward` binary against a stub-backend `wardd`.

mod common;

use std::process::Output;
use std::time::Duration;

use common::Daemon;

const ALPINE: &str = "alpine:latest";
/// Sandboxes are ordered by whole seconds of creation time.
const CREATION_GAP: Duration = Duration::from_millis(1100);

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout(&out)
}

fn create(d: &Daemon, name: &str) -> String {
    ok(d, &["create", "--name", name, ALPINE])
        .trim()
        .to_string()
}

#[test]
fn given_a_sandbox_and_an_image_when_exists_runs_then_the_exit_code_answers_and_nothing_prints() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, "web1");

    // Act
    let by_name = run(&d, &["container", "exists", "web1"]);
    let by_prefix = run(&d, &["container", "exists", &id[..8]]);
    let missing = run(&d, &["container", "exists", "nope"]);
    let image = run(&d, &["image", "exists", ALPINE]);
    let no_image = run(&d, &["image", "exists", "nosuchimage:1"]);

    // Assert
    assert_eq!(by_name.status.code(), Some(0));
    assert_eq!(by_prefix.status.code(), Some(0));
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(image.status.code(), Some(0));
    assert_eq!(no_image.status.code(), Some(1));
    for out in [&by_name, &missing, &image, &no_image] {
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}

#[test]
fn given_two_sandboxes_when_ps_sorted_then_the_order_follows_the_field() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "zulu");
    std::thread::sleep(CREATION_GAP);
    create(&d, "alpha");

    // Act
    let names = ok(&d, &["ps", "--sort", "names", "--format", "{{.Names}}"]);
    let created = ok(&d, &["ps", "--sort", "created", "--format", "{{.Names}}"]);
    let running_for = ok(
        &d,
        &["ps", "--sort", "runningfor", "--format", "{{.Names}}"],
    );
    let bad = run(&d, &["ps", "--sort", "size"]);

    // Assert
    assert_eq!(names, "alpha\nzulu\n");
    assert_eq!(created, "zulu\nalpha\n");
    assert_eq!(running_for, "alpha\nzulu\n");
    assert!(!bad.status.success());
}

#[test]
fn given_two_sandboxes_when_latest_is_used_then_the_newest_is_acted_on() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "old");
    std::thread::sleep(CREATION_GAP);
    create(&d, "new");

    // Act
    let name = ok(&d, &["inspect", "-l", "--format", "{{.Name}}"]);
    let group = ok(&d, &["container", "inspect", "--latest", "-f", "{{.Name}}"]);
    ok(&d, &["stop", "-l"]);
    let running = ok(&d, &["ps", "--format", "{{.Names}}"]);
    ok(&d, &["rm", "-l"]);
    let left = ok(&d, &["ps", "-a", "--format", "{{.Names}}"]);

    // Assert
    assert_eq!(name.trim(), "/new");
    assert_eq!(group.trim(), "/new");
    assert_eq!(running.trim(), "old");
    assert_eq!(left.trim(), "old");
}

#[test]
fn given_two_daemons_when_latest_is_used_with_a_socket_flag_then_that_daemon_answers() {
    // Arrange
    let with_sandbox = Daemon::spawn();
    let empty = Daemon::spawn();
    create(&with_sandbox, "only-here");
    let socket = with_sandbox.socket.to_string_lossy().into_owned();
    let query = |args: &[&str]| {
        let out = empty.cli().args(args).output().expect("run ward");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // Act
    let separate = query(&["--socket", &socket, "inspect", "-l", "-f", "{{.Name}}"]);
    let joined = query(&[
        &format!("--socket={socket}"),
        "inspect",
        "-l",
        "-f",
        "{{.Name}}",
    ]);
    let after = query(&["inspect", "-l", "-f", "{{.Name}}", "--socket", &socket]);
    let env_only = with_sandbox
        .cli()
        .args(["inspect", "-l", "-f", "{{.Name}}"])
        .output()
        .expect("run ward");

    // Assert
    assert_eq!(separate.trim(), "/only-here");
    assert_eq!(joined.trim(), "/only-here");
    assert_eq!(after.trim(), "/only-here");
    assert_eq!(
        String::from_utf8_lossy(&env_only.stdout).trim(),
        "/only-here"
    );
}

#[test]
fn given_missing_arguments_when_exists_is_used_then_the_usage_error_exits_125() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let container = run(&d, &["container", "exists"]);
    let image = run(&d, &["image", "exists"]);
    let bare_digest = run(&d, &["image", "exists", "sha256:"]);

    // Assert
    assert_eq!(container.status.code(), Some(125));
    assert_eq!(image.status.code(), Some(125));
    assert_eq!(bare_digest.status.code(), Some(1));
}

#[test]
fn given_no_sandbox_when_latest_is_used_then_it_fails_with_a_reason() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["stop", "-l"]);

    // Assert
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sandbox"));
}
