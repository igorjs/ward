// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the create-time flags of `ward run` and `ward create`:
//! labels, `--cidfile`, `--replace`, `--platform`, `--init` and env files.
//! They run the real `ward` binary against a stub-backend `wardd`.

mod common;

use std::process::Output;

use common::Daemon;

const ALPINE: &str = "alpine:latest";
const COMMAND: &str = "true";
/// Characters of the sandbox ID that make the default host name.
const SHORT_ID_LEN: usize = 12;

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

fn host_platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("linux/{arch}")
}

#[test]
fn given_labels_when_filtered_and_formatted_then_ps_and_inspect_show_them() {
    // Arrange
    let d = Daemon::spawn();
    let tagged = ok(
        &d,
        &[
            "create",
            "--name",
            "web1",
            "-l",
            "team=infra",
            "-l",
            "flag",
            ALPINE,
        ],
    )
    .trim()
    .to_string();
    ok(
        &d,
        &["create", "--name", "cache1", "-l", "team=data", ALPINE],
    );

    // Act
    let by_value = ok(
        &d,
        &[
            "ps",
            "-a",
            "-q",
            "--no-trunc",
            "--filter",
            "label=team=infra",
        ],
    );
    let by_key = ok(&d, &["ps", "-a", "-q", "--filter", "label=flag"]);
    let none = ok(&d, &["ps", "-a", "-q", "--filter", "label=team=ops"]);
    let both = ok(
        &d,
        &[
            "ps",
            "-a",
            "--filter",
            "label=team",
            "--filter",
            "label=flag",
            "--format",
            "{{.Names}}",
        ],
    );
    let joined = ok(
        &d,
        &[
            "ps",
            "-a",
            "--filter",
            "name=web1",
            "--format",
            "{{.Labels}}",
        ],
    );
    let one = ok(
        &d,
        &[
            "ps",
            "-a",
            "--filter",
            "name=web1",
            "--format",
            "{{.Label \"team\"}}",
        ],
    );
    let config = ok(
        &d,
        &["inspect", "--format", "{{json .Config.Labels}}", "web1"],
    );

    // Assert
    assert_eq!(by_value.trim(), tagged);
    assert_eq!(by_key.trim().len(), 12);
    assert!(none.trim().is_empty());
    assert_eq!(both.trim(), "web1");
    assert_eq!(joined.trim(), "flag=,team=infra");
    assert_eq!(one.trim(), "infra");
    assert_eq!(config.trim(), r#"{"flag":"","team":"infra"}"#);
}

#[test]
fn given_label_file_and_flag_when_created_then_the_flag_wins() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("labels");
    std::fs::write(&file, "# labels\na=file\nb=file\n").expect("write");

    // Act
    ok(
        &d,
        &[
            "create",
            "--name",
            "web1",
            "--label-file",
            file.to_str().unwrap(),
            "-l",
            "b=flag",
            ALPINE,
        ],
    );
    let config = ok(
        &d,
        &["inspect", "--format", "{{json .Config.Labels}}", "web1"],
    );

    // Assert
    assert_eq!(config.trim(), r#"{"a":"file","b":"flag"}"#);
}

#[test]
fn given_labeled_sandbox_when_events_filtered_by_label_then_its_events_match() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &["create", "--name", "web1", "-l", "team=infra", ALPINE],
    );
    ok(&d, &["create", "--name", "cache1", ALPINE]);

    // Act
    let events = ok(
        &d,
        &[
            "events",
            "--since",
            "1h",
            "--until",
            "0s",
            "--filter",
            "label=team=infra",
            "--filter",
            "event=create",
            "--format",
            "{{.Actor.Attributes.name}}",
        ],
    );

    // Assert
    assert_eq!(events.trim(), "web1");
}

#[test]
fn given_cidfile_when_run_detached_then_it_holds_the_id_and_a_second_run_is_refused() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cid");
    let path = path.to_str().unwrap();

    // Act
    let id = ok(&d, &["run", "-d", "--cidfile", path, ALPINE, COMMAND]);
    let second = run(&d, &["run", "-d", "--cidfile", path, ALPINE, COMMAND]);

    // Assert
    assert_eq!(std::fs::read_to_string(path).expect("cidfile"), id.trim());
    assert!(!second.status.success());
    assert!(
        stderr(&second).contains("ID file found"),
        "{}",
        stderr(&second)
    );
}

#[test]
fn given_failed_create_when_cidfile_given_then_no_file_is_left() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cid");

    // Act
    let out = run(
        &d,
        &[
            "create",
            "--cidfile",
            path.to_str().unwrap(),
            "--egress",
            "bogus",
            ALPINE,
        ],
    );
    let bad_image = run(
        &d,
        &["create", "--cidfile", path.to_str().unwrap(), "bad image!"],
    );

    // Assert
    assert!(!out.status.success());
    assert!(!bad_image.status.success());
    assert!(!path.exists());
}

#[test]
fn given_replace_when_name_is_taken_then_the_old_sandbox_is_removed() {
    // Arrange
    let d = Daemon::spawn();
    let old = ok(&d, &["create", "--name", "web1", ALPINE])
        .trim()
        .to_string();

    // Act
    let without = run(&d, &["create", "--name", "web1", ALPINE]);
    let new = ok(&d, &["create", "--replace", "--name", "web1", ALPINE])
        .trim()
        .to_string();
    let listed = ok(&d, &["ps", "-a", "-q", "--no-trunc"]);

    // Assert
    assert!(!without.status.success());
    assert_ne!(old, new);
    assert_eq!(listed.trim(), new);
}

#[test]
fn given_replace_when_name_is_free_then_the_sandbox_is_created() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let id = ok(&d, &["create", "--replace", "--name", "fresh", ALPINE]);

    // Assert
    assert!(!id.trim().is_empty());
}

#[test]
fn given_platform_when_it_is_the_hosts_then_create_works_and_other_platforms_fail() {
    // Arrange
    let d = Daemon::spawn();
    let host = host_platform();

    // Act
    let accepted = run(&d, &["create", "--platform", &host, ALPINE]);
    let refused = run(&d, &["create", "--platform", "linux/s390x", ALPINE]);

    // Assert
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("--platform linux/s390x"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn given_init_flag_when_created_then_it_is_accepted() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["create", "--init", ALPINE]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn given_missing_env_file_when_created_then_it_fails_naming_the_file() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["create", "--env-file", "/nonexistent/x.env", ALPINE]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("/nonexistent/x.env"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_env_file_when_created_then_it_is_accepted() {
    // Arrange
    let d = Daemon::spawn();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("app.env");
    std::fs::write(&file, "# comment\nA=1\n\nB=2\n").expect("write");

    // Act
    let out = run(
        &d,
        &[
            "create",
            "--env-file",
            file.to_str().unwrap(),
            "-e",
            "A=3",
            ALPINE,
        ],
    );

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn given_hostname_when_inspected_then_config_shows_it_or_the_short_id() {
    // Arrange
    let d = Daemon::spawn();
    let named = ok(&d, &["create", "--hostname", "web.example.com", ALPINE])
        .trim()
        .to_string();
    let plain = ok(&d, &["create", ALPINE]).trim().to_string();

    // Act
    let custom = ok(&d, &["inspect", "--format", "{{.Config.Hostname}}", &named]);
    let default = ok(&d, &["inspect", "--format", "{{.Config.Hostname}}", &plain]);

    // Assert
    assert_eq!(custom.trim(), "web.example.com");
    assert_eq!(default.trim(), &plain[..SHORT_ID_LEN]);
}

#[test]
fn given_stop_settings_when_inspected_then_config_shows_them() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &[
            "create",
            "--name",
            "web1",
            "--stop-signal",
            "SIGINT",
            "--stop-timeout",
            "30",
            ALPINE,
        ],
    );
    ok(&d, &["create", "--name", "plain1", ALPINE]);

    // Act
    let set = ok(
        &d,
        &[
            "inspect",
            "--format",
            "{{.Config.StopSignal}} {{.Config.StopTimeout}}",
            "web1",
        ],
    );
    let unset = ok(
        &d,
        &["inspect", "--format", "[{{.Config.StopSignal}}]", "plain1"],
    );

    // Assert
    assert_eq!(set.trim(), "SIGINT 30");
    assert_eq!(unset.trim(), "[]");
}

#[test]
fn given_bad_hostname_or_stop_signal_when_created_then_it_fails_with_a_clear_error() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let host = run(&d, &["create", "--hostname=-bad", ALPINE]);
    let signal = run(&d, &["create", "--stop-signal", "SIGNOPE", ALPINE]);

    // Assert
    assert!(!host.status.success());
    assert!(stderr(&host).contains("hostname"), "{}", stderr(&host));
    assert!(!signal.status.success());
    assert!(
        stderr(&signal).contains("--stop-signal"),
        "{}",
        stderr(&signal)
    );
}
