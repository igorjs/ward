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
    assert!(header.ends_with("\tNAMES\tSIZE"), "{header}");
}

#[test]
fn given_a_sandbox_when_info_format_then_docker_named_fields_render() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["create", "--name", "web1", ALPINE]);

    // Act
    let line = ok(
        &d,
        &[
            "system",
            "info",
            "--format",
            "{{.Containers}} {{.ContainersRunning}} {{.ContainersStopped}} {{.OSType}}",
        ],
    );

    // Assert
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(&fields[..3], ["1", "1", "0"]);
    assert!(!fields[3].is_empty(), "{line}");
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
