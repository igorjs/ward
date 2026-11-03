// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the `prune` commands: the confirmation prompt, the
//! filters and the report. They run the real `ward` binary against a
//! stub-backend `wardd`.

mod common;

use std::io::Write;
use std::process::{Output, Stdio};

use common::Daemon;

const ALPINE: &str = "alpine:latest";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn run_with_input(d: &Daemon, args: &[&str], input: &str) -> Output {
    let mut child = d
        .cli()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ward");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(input.as_bytes()).expect("write answer");
    drop(stdin);
    child.wait_with_output().expect("wait for ward")
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

fn stopped(d: &Daemon, name: &str, extra: &[&str]) -> String {
    let mut args = vec!["create", "--name", name];
    args.extend_from_slice(extra);
    args.push(ALPINE);
    let id = ok(d, &args).trim().to_string();
    ok(d, &["stop", name]);
    id
}

fn names(d: &Daemon) -> String {
    ok(d, &["ps", "-a", "--format", "{{.Names}}"])
}

#[test]
fn given_no_force_when_container_prune_then_only_a_y_answer_removes_sandboxes() {
    // Arrange
    let d = Daemon::spawn();
    let id = stopped(&d, "old1", &[]);

    // Act
    let closed = run(&d, &["container", "prune"]);
    let declined = run_with_input(&d, &["container", "prune"], "n\n");
    let kept = names(&d);
    let accepted = run_with_input(&d, &["container", "prune"], "y\n");

    // Assert
    assert!(!closed.status.success());
    assert!(
        stderr(&closed).contains("prune cancelled"),
        "{}",
        stderr(&closed)
    );
    assert!(stdout(&closed).contains("Are you sure you want to continue? [y/N] "));
    assert!(!declined.status.success());
    assert_eq!(kept, "old1\n");
    assert!(accepted.status.success(), "{}", stderr(&accepted));
    assert!(stdout(&accepted).contains(&format!("Deleted Containers:\n{id}\n")));
    assert!(stdout(&accepted).contains("Total reclaimed space: 0B"));
    assert_eq!(names(&d), "");
}

#[test]
fn given_filters_when_container_prune_forced_then_only_matching_sandboxes_go() {
    // Arrange
    let d = Daemon::spawn();
    stopped(&d, "keepme", &["-l", "tier=db"]);
    stopped(&d, "dropme", &["-l", "tier=web"]);

    // Act
    let too_new = ok(&d, &["container", "prune", "-f", "--filter", "until=1h"]);
    let negated = ok(
        &d,
        &["container", "prune", "-f", "--filter", "label!=tier=db"],
    );
    let bad = run(&d, &["container", "prune", "-f", "--filter", "colour=red"]);

    // Assert
    assert!(!too_new.contains("Deleted Containers"), "{too_new}");
    assert!(negated.contains("Deleted Containers"), "{negated}");
    assert_eq!(names(&d), "keepme\n");
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("invalid filter"), "{}", stderr(&bad));
}

#[test]
fn given_an_unused_image_when_image_prune_then_only_all_removes_it() {
    // Arrange
    let d = Daemon::spawn();
    let id = stopped(&d, "old1", &[]);
    ok(&d, &["rm", &id]);

    // Act
    let dangling = ok(&d, &["image", "prune", "-f"]);
    let used = {
        ok(&d, &["create", "--name", "web1", ALPINE]);
        ok(&d, &["image", "prune", "-a", "-f"])
    };
    ok(&d, &["rm", "-f", "web1"]);
    let all = ok(&d, &["image", "prune", "-a", "-f"]);

    // Assert
    assert_eq!(dangling, "Total reclaimed space: 0B\n");
    assert!(!used.contains("untagged"), "{used}");
    assert!(all.contains("untagged: alpine:latest"), "{all}");
    assert_ne!(run(&d, &["image", "exists", ALPINE]).status.code(), Some(0));
}

#[test]
fn given_a_label_filter_on_a_kind_without_labels_when_pruned_then_it_is_refused() {
    // Arrange
    let d = Daemon::spawn();
    stopped(&d, "old1", &[]);

    // Act
    let outs = [
        run(&d, &["image", "prune", "-a", "-f", "--filter", "label!=a"]),
        run(&d, &["volume", "prune", "-f", "--filter", "label=a"]),
        run(&d, &["system", "prune", "-f", "--filter", "label=a"]),
    ];

    // Assert
    for out in &outs {
        assert!(!out.status.success());
        assert!(
            stderr(out).contains("only supported by container prune"),
            "{}",
            stderr(out)
        );
    }
    assert_eq!(names(&d), "old1\n");
}

#[test]
fn given_a_recent_sandbox_when_pruned_by_until_then_it_is_kept() {
    // Arrange
    let d = Daemon::spawn();
    stopped(&d, "fresh", &[]);

    // Act
    let out = ok(&d, &["container", "prune", "-f", "--filter", "until=1h"]);

    // Assert
    assert!(!out.contains("Deleted Containers"), "{out}");
    assert_eq!(names(&d), "fresh\n");
}

#[test]
fn given_a_stopped_sandbox_without_a_token_when_pruned_then_it_stays_and_the_prune_fails() {
    // Arrange
    let d = Daemon::spawn();
    stopped(&d, "tokenless", &[]);
    let store = d.socket.parent().expect("socket dir").join("tokens.json");
    std::fs::remove_file(&store).expect("remove token store");

    // Act
    let out = run(&d, &["container", "prune", "-f"]);

    // Assert
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no token"), "{}", stderr(&out));
    assert!(names(&d).contains("tokenless"));
}

#[test]
fn given_an_unreadable_token_store_and_only_running_sandboxes_when_pruned_then_it_succeeds() {
    // Arrange
    let d = Daemon::spawn();
    ok(&d, &["run", "-d", "--name", "busy", ALPINE, "sleep", "300"]);
    let store = d.socket.parent().expect("socket dir").join("tokens.json");
    std::fs::write(&store, "not json").expect("corrupt token store");

    // Act
    let out = run(&d, &["container", "prune", "-f"]);

    // Assert
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(names(&d).contains("busy"));
}

#[test]
fn given_a_stopped_sandbox_when_system_prune_forced_then_it_is_removed_and_reported() {
    // Arrange
    let d = Daemon::spawn();
    let id = stopped(&d, "old1", &[]);

    // Act
    let out = ok(&d, &["system", "prune", "-f"]);
    let json = ok(&d, &["--json", "system", "prune", "-f"]);

    // Assert
    assert!(
        out.contains(&format!("Deleted Containers:\n{id}\n")),
        "{out}"
    );
    assert!(json.contains("\"ContainersDeleted\":[]"), "{json}");
}
