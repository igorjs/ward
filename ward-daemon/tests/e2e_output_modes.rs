// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E coverage for the CLI's output modes (igorjs/ward#18 + #19).
//!
//! Three modes per command family:
//!
//!   1. Default key/value or tab-separated (the historical behaviour;
//!      stable for `grep` / `awk` pipelines).
//!   2. `--json` (or `WARD_JSON=1`): one JSON object per command,
//!      array for list commands. Stable for `jq`.
//!   3. List commands on a TTY: aligned tables via `tabled`. Piped
//!      output stays tab-separated even without `--no-pretty` because
//!      assert_cmd's child stdout is not a TTY; this matches the
//!      isatty(stdout) detection ward-cli does.
//!
//! Style mirrors the rest of `ward-daemon/tests/`: BDD names, one
//! scenario per test, shared `common::Daemon` for daemon isolation.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;

#[test]
fn given_create_with_json_flag_when_run_then_emits_single_json_object() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["--json", "create", "alpine:latest"]).assert();

    // Assert: one valid JSON line containing the documented keys.
    // We don't pin the id (UUID-random) but every other field is
    // deterministic for a freshly-created sandbox.
    assertion.success().stdout(predicate::function(|s: &str| {
        let parsed: serde_json::Value = serde_json::from_str(s.trim())
            .expect("--json stdout must be a single valid JSON object");
        parsed.get("id").and_then(|v| v.as_str()).is_some()
            && parsed.get("status").and_then(|v| v.as_str()) == Some("creating")
            && parsed.get("image").and_then(|v| v.as_str()) == Some("alpine:latest")
    }));
}

#[test]
fn given_create_without_json_when_run_then_emits_key_value_lines() {
    // Arrange
    let daemon = common::Daemon::spawn();

    // Act: no --json flag.
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["create", "alpine:latest"]).assert();

    // Assert: historical key/value lines still work, byte-for-byte
    // compatible with existing scripts.
    assertion
        .success()
        .stdout(predicate::str::contains("id:"))
        .stdout(predicate::str::contains("status: creating"))
        .stdout(predicate::str::contains("image: alpine:latest"))
        // Negative: no JSON braces leak into the default path.
        .stdout(predicate::str::contains("{").not());
}

#[test]
fn given_list_with_json_flag_when_run_then_emits_array() {
    // Arrange: create two sandboxes so we get a non-trivial array.
    let daemon = common::Daemon::spawn();
    daemon
        .cli()
        .args(["create", "alpine:latest"])
        .assert()
        .success();
    daemon
        .cli()
        .args(["create", "alpine:3.19"])
        .assert()
        .success();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["--json", "list"]).assert();

    // Assert
    assertion.success().stdout(predicate::function(|s: &str| {
        let parsed: serde_json::Value = match serde_json::from_str(s.trim()) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let arr = match parsed.as_array() {
            Some(a) => a,
            None => return false,
        };
        arr.len() == 2
            && arr.iter().all(|entry| {
                entry.get("id").and_then(|v| v.as_str()).is_some()
                    && entry.get("status").and_then(|v| v.as_str()).is_some()
                    && entry.get("image").and_then(|v| v.as_str()).is_some()
            })
    }));
}

#[test]
fn given_list_when_run_piped_then_emits_tab_separated() {
    // Arrange
    let daemon = common::Daemon::spawn();
    daemon
        .cli()
        .args(["create", "alpine:latest"])
        .assert()
        .success();

    // Act: assert_cmd's child stdout is not a TTY, so the CLI's
    // isatty(stdout) check returns false and the default path stays
    // tab-separated even without --no-pretty.
    let mut cmd = daemon.cli();
    let assertion = cmd.arg("list").assert();

    // Assert: exactly one row, three tab-separated columns.
    assertion.success().stdout(predicate::function(|s: &str| {
        let lines: Vec<&str> = s.lines().collect();
        lines.len() == 1 && lines[0].matches('\t').count() == 2
    }));
}

#[test]
fn given_empty_list_with_json_when_run_then_emits_empty_array() {
    // Arrange: no sandboxes.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["--json", "list"]).assert();

    // Assert: literal `[]` (one line, no trailing data). Scripts can
    // `jq 'length'` against this without special-casing empty.
    assertion.success().stdout("[]\n");
}

// NOTE: an analogous `ward volume list --json` test would belong here but
// `ward volume create` shells out to `mkfs.ext4` which is Linux-only;
// macOS dev hosts cannot drive this path. The same shape is already
// covered for sandboxes by `given_list_with_json_flag_when_run_then_emits_array`
// above (same emit_rows helper, same JSON-array path), so coverage of
// the list-JSON code path is not gated on the unfixable platform skew.

#[test]
fn given_remove_with_json_when_run_then_emits_removed_key() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let create = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    let stdout = String::from_utf8(create.stdout).expect("utf8");
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("id: "))
        .expect("id line")
        .trim()
        .to_string();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["--json", "remove", &id]).assert();

    // Assert: `{"removed": "<id>"}` on one line.
    assertion
        .success()
        .stdout(predicate::function(move |s: &str| {
            let parsed: serde_json::Value = match serde_json::from_str(s.trim()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            parsed.get("removed").and_then(|v| v.as_str()) == Some(id.as_str())
        }));
}

#[test]
fn given_ward_json_env_when_set_then_command_emits_json() {
    // Arrange: WARD_JSON=1 mirrors the --json flag for use in
    // long-running shell sessions where exporting once beats
    // remembering to add the flag per command.
    let daemon = common::Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .env("WARD_JSON", "1")
        .args(["create", "alpine:latest"])
        .assert();

    // Assert: JSON object, same shape as the --json flag path.
    assertion.success().stdout(predicate::function(|s: &str| {
        serde_json::from_str::<serde_json::Value>(s.trim()).is_ok()
    }));
}
