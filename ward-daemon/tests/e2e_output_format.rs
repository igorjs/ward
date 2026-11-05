// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for Go template `--format` output and the Podman extras.
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
fn given_go_template_syntax_when_inspect_and_ps_format_then_range_if_and_functions_work() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &["create", "--name", "web1", "-l", "b=2", "-l", "a=1", ALPINE],
    );

    // Act
    let labels = ok(
        &d,
        &[
            "inspect",
            "-f",
            "{{range $k, $v := .Config.Labels}}{{$k}}={{$v}};{{end}}",
            "web1",
        ],
    );
    let map = ok(&d, &["inspect", "-f", "{{.Config.Labels}}", "web1"]);
    let state = ok(
        &d,
        &[
            "inspect",
            "-f",
            "{{if eq .State.Status \"running\"}}{{upper .State.Status}}{{else}}other{{end}}",
            "web1",
        ],
    );
    let ps = ok(
        &d,
        &[
            "ps",
            "-a",
            "--format",
            "{{.Names}}|{{.Ports}}|{{.Mounts}}|{{.Networks}}|{{.Label \"a\"}}",
        ],
    );
    let sized = ok(&d, &["ps", "-a", "-s"]);

    // Assert
    assert_eq!(labels.trim(), "a=1;b=2;");
    assert_eq!(map.trim(), "map[a:1 b:2]");
    assert_eq!(state.trim(), "RUNNING");
    assert_eq!(ps.trim(), "web1||||1");
    let header = sized.lines().next().expect("header");
    assert!(header.ends_with("NAMES   SIZE"), "{header}");
    let row = sized.lines().nth(1).expect("row");
    assert!(row.contains("-- (virtual "), "{row}");
}

#[test]
fn given_an_untagged_image_name_when_ps_size_then_the_cached_tag_is_found() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web1", "alpine"]);

    // Act
    let sized = ok(&d, &["ps", "-a", "-s", "--format", "{{.Names}} {{.Size}}"]);

    // Assert
    assert!(sized.contains("web1 -- (virtual "), "{sized}");
}

#[test]
fn given_a_sandbox_when_info_format_then_docker_named_fields_render() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web1", ALPINE]);
    let format = "{{.Containers}} {{.ContainersRunning}} {{.ContainersStopped}} {{.OSType}}";

    // Act
    let created = ok(&d, &["system", "info", "--format", format]);
    let status = ok(&d, &["inspect", "-f", "{{.State.Status}}", "web1"]);
    ok(&d, &["stop", "web1"]);
    let stopped = ok(&d, &["system", "info", "--format", format]);

    // Assert
    // Ward starts a sandbox on create, so it counts as running right away.
    assert_eq!(status.trim(), "running");
    let fields: Vec<&str> = created.split_whitespace().collect();
    assert_eq!(&fields[..3], ["1", "1", "0"]);
    assert!(!fields[3].is_empty(), "{created}");
    let fields: Vec<&str> = stopped.split_whitespace().collect();
    assert_eq!(&fields[..3], ["1", "0", "1"]);
}

#[test]
fn given_a_bad_template_when_ps_format_then_it_fails_before_listing() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["ps", "--format", "{{nosuchfunc .ID}}"]);

    // Assert
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nosuchfunc"), "{}", stderr(&out));
}
