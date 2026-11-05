// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the Docker shaped command line: the `container` and
//! `image` groups, `version`, `system`, `rename`, and the `ps` and `inspect`
//! flags. They run the real `ward` binary against a stub-backend `wardd`.

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

fn create(d: &Daemon, name: &str) -> String {
    ok(d, &["container", "create", "--name", name, ALPINE])
        .trim()
        .to_string()
}

#[test]
fn given_container_group_when_create_ls_rm_then_they_act_like_the_top_level_commands() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, "web1");

    // Act
    let listed = ok(&d, &["container", "ls", "-a", "--no-trunc", "-q"]);
    ok(&d, &["container", "rm", "-f", "web1"]);

    // Assert
    assert_eq!(listed.trim(), id);
    assert_eq!(ok(&d, &["container", "ps", "-a", "-q"]).trim(), "");
}

#[test]
fn given_unknown_container_subcommand_when_run_then_it_fails_with_a_hint() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = run(&d, &["container", "nope"]);

    // Assert
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("ward container --help"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn given_image_group_when_ls_then_it_matches_images() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let group = ok(&d, &["image", "ls"]);
    let top = ok(&d, &["images"]);

    // Assert
    assert_eq!(group, top);
}

#[test]
fn given_daemon_when_version_then_client_and_server_print_and_format_applies() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let plain = ok(&d, &["version"]);
    let formatted = ok(&d, &["version", "--format", "{{.Server.Backend}}"]);
    let json: serde_json::Value =
        serde_json::from_str(&ok(&d, &["--json", "version"])).expect("json");

    // Assert
    assert!(plain.starts_with("Client:\n Version:"), "{plain}");
    assert!(plain.contains("\nServer:\n"), "{plain}");
    assert!(!formatted.trim().is_empty());
    assert_eq!(json["Server"]["Backend"].as_str(), Some(formatted.trim()));
}

#[test]
fn given_no_daemon_when_version_then_client_prints_and_command_fails() {
    // Arrange
    let d = Daemon::spawn();
    let mut cmd = d.cli();
    cmd.args(["--socket", "/nonexistent/ward.sock", "version"]);

    // Act
    let out = cmd.output().expect("run ward");

    // Assert
    assert!(!out.status.success());
    assert!(stdout(&out).starts_with("Client:"), "{}", stdout(&out));
}

#[test]
fn given_system_group_when_info_df_prune_then_each_runs() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, "old1");
    ok(&d, &["stop", "old1"]);

    // Act
    let info = ok(&d, &["system", "info"]);
    let df = ok(&d, &["--no-pretty", "system", "df"]);
    let pruned = ok(&d, &["system", "prune", "-f"]);

    // Assert
    assert!(info.contains("Isolation: microVM"), "{info}");
    assert!(info.contains("Containers: 1"), "{info}");
    assert!(df.contains("Containers\t1\t0"), "{df}");
    assert!(pruned.contains(&id), "{pruned}");
}

#[test]
fn given_named_sandbox_when_renamed_then_new_name_works_and_old_is_gone() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    create(&d, "web2");

    // Act
    let silent = ok(&d, &["rename", "web1", "api"]);
    let taken = run(&d, &["container", "rename", "api", "web2"]);
    let old = run(&d, &["inspect", "web1"]);

    // Assert
    assert_eq!(silent, "");
    assert!(!taken.status.success());
    assert!(stderr(&taken).contains("web2"), "{}", stderr(&taken));
    assert!(!old.status.success());
    assert_eq!(
        ok(&d, &["inspect", "-f", "{{.Name}}", "api"]).trim(),
        "/api"
    );
}

#[test]
fn given_rename_when_events_replayed_then_a_rename_event_is_reported() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");
    ok(&d, &["rename", "web1", "api"]);

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
            "event=rename",
            "--format",
            "{{.Action}}",
        ],
    );

    // Assert
    assert_eq!(events.trim(), "rename");
}

#[test]
fn given_sandboxes_when_ps_filtered_then_name_id_and_status_narrow_the_list() {
    // Arrange
    let d = Daemon::spawn();
    let web = create(&d, "web1");
    create(&d, "cache1");
    ok(&d, &["stop", "web1"]);

    // Act
    let by_name = ok(&d, &["ps", "-a", "-q", "--filter", "name=web"]);
    let by_id = ok(
        &d,
        &["ps", "-a", "-q", "--filter", &format!("id={}", &web[..8])],
    );
    let by_status = ok(
        &d,
        &["ps", "-q", "--filter", "status=stopped", "--no-trunc"],
    );
    let bad = run(&d, &["ps", "--filter", "color=red"]);

    // Assert
    assert_eq!(by_name.trim(), &web[..12]);
    assert_eq!(by_id.trim(), &web[..12]);
    assert_eq!(by_status.trim(), web);
    assert!(!bad.status.success());
}

#[test]
fn given_three_sandboxes_when_ps_last_and_latest_then_newest_come_first() {
    // Arrange
    let d = Daemon::spawn();
    for name in ["one", "two", "three"] {
        create(&d, name);
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    // Act
    let latest = ok(&d, &["ps", "-l", "--format", "{{.Names}}"]);
    let last_two = ok(&d, &["ps", "-n", "2", "--format", "{{.Names}}"]);
    let table = ok(
        &d,
        &["ps", "-a", "--format", "table {{.Names}}\t{{.State}}"],
    );

    // Assert
    assert_eq!(latest, "three\n");
    assert_eq!(last_two, "three\ntwo\n");
    assert!(table.starts_with("NAMES"), "{table}");
    assert!(table.contains("running"), "{table}");
}

#[test]
fn given_sandbox_when_inspect_format_and_type_then_template_renders_and_image_type_is_refused() {
    // Arrange
    let d = Daemon::spawn();
    let id = create(&d, "web1");

    // Act
    let name = ok(
        &d,
        &["inspect", "--type", "container", "-f", "{{.Name}}", "web1"],
    );
    let config = ok(&d, &["inspect", "-f", "{{json .Config.Image}}", "web1"]);
    let image = run(&d, &["inspect", "--type", "image", "web1"]);

    // Assert
    assert_eq!(name.trim(), "/web1");
    assert_eq!(config.trim(), format!("\"{ALPINE}\""));
    assert!(!image.status.success());
    assert!(
        stderr(&image).contains("not supported"),
        "{}",
        stderr(&image)
    );
    assert!(!id.is_empty());
}

#[test]
fn given_unknown_signal_when_kill_then_it_is_refused_before_any_call() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");

    // Act
    let bad = run(&d, &["kill", "-s", "SIGNOPE", "web1"]);

    // Assert
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("invalid signal"), "{}", stderr(&bad));
}

#[test]
fn given_info_format_when_host_fields_asked_then_the_daemon_reports_them() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = ok(
        &d,
        &[
            "info",
            "--format",
            "{{.Name}}|{{.NCPU}}|{{.DockerRootDir}}|{{.ID}}|{{.ContainersPaused}}",
        ],
    );
    let again = ok(&d, &["info", "--format", "{{.ID}}"]);

    // Assert
    let fields: Vec<&str> = out.trim().split('|').collect();
    assert!(!fields[0].is_empty(), "{out}");
    assert!(fields[1].parse::<u32>().expect("ncpu") > 0, "{out}");
    assert!(!fields[2].is_empty(), "{out}");
    assert!(!fields[3].is_empty(), "{out}");
    assert_eq!(fields[3], again.trim());
    assert_eq!(fields[4], "0");
}

#[test]
fn given_system_df_when_run_then_docker_s_table_and_format_print() {
    // Arrange
    let d = Daemon::spawn();
    create(&d, "web1");

    // Act
    let table = ok(&d, &["system", "df"]);
    let kinds = ok(
        &d,
        &["system", "df", "--format", "{{.Type}}={{.TotalCount}}"],
    );

    // Assert
    let header = table.lines().next().expect("header");
    assert!(header.starts_with("TYPE "), "{table}");
    assert!(header.ends_with("   RECLAIMABLE"), "{table}");
    assert!(kinds.contains("Containers=1"), "{kinds}");
    assert!(kinds.contains("Build Cache=0"), "{kinds}");
}
