// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for `ward run`, `ward exec -it` and `ward logs` over Attach.
//!
//! The stub backend prints `stub: <argv0>` and exits 0 for every command, so
//! exit codes other than 0 are only checked against a real guest
//! (`--features krunvm` with `WARD_E2E_VM=1`).

mod common;

#[cfg(not(feature = "krunvm"))]
use std::io::Write;
use std::process::Stdio;
#[cfg(not(feature = "krunvm"))]
use std::time::{Duration, Instant};

#[cfg(not(feature = "krunvm"))]
use assert_cmd::prelude::*;
use common::Daemon;
#[cfg(not(feature = "krunvm"))]
use predicates::prelude::*;
#[cfg(not(feature = "krunvm"))]
use tonic::Code;
#[cfg(not(feature = "krunvm"))]
use ward_core::pb::RemoveSandboxRequest;

#[cfg(not(feature = "krunvm"))]
const POLL_STEP: Duration = Duration::from_millis(100);
#[cfg(not(feature = "krunvm"))]
const POLL_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(not(feature = "krunvm"))]
const NOT_A_TTY: &str = "the input device is not a TTY";
const DRAIN_BUFFER_BYTES: usize = 256;
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";

/// The image real-VM runs use; CI points it at a registry the runner allows.
fn image() -> String {
    std::env::var(IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn create_sandbox(daemon: &Daemon) -> String {
    let mut cmd = daemon.cli();
    cmd.args(["create", &image()]);
    let out = cmd.output().expect("create");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("id: "))
        .expect("id line")
        .to_string()
}

#[cfg(not(feature = "krunvm"))]
/// Run `ward logs` until its output holds `needle`, since the stub prints
/// shortly after the sandbox is created.
fn logs_until(daemon: &Daemon, args: &[&str], needle: &str) -> String {
    let deadline = Instant::now() + POLL_DEADLINE;
    loop {
        let out = daemon.cli().args(args).output().expect("logs");
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success(), "logs failed: {out:?}");
        if text.contains(needle) || Instant::now() > deadline {
            return text;
        }
        std::thread::sleep(POLL_STEP);
    }
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_run_with_a_command_when_it_completes_then_its_output_is_printed_and_exit_is_zero() {
    // Arrange
    let daemon = Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["run", "alpine", "echo", "hi"]).assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::contains("stub: echo"));
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_run_t_when_stdin_is_not_a_terminal_then_it_fails_with_the_docker_message() {
    // Arrange
    let daemon = Daemon::spawn();

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd
        .args(["run", "-t", "alpine", "true"])
        .stdin(Stdio::null())
        .assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains(NOT_A_TTY));
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_run_rm_when_it_completes_then_a_second_remove_is_not_found() {
    // Arrange
    let daemon = Daemon::spawn();
    let name = "rm-after-run";

    // Act
    let status = daemon
        .cli()
        .args(["run", "--rm", "--name", name, "alpine", "true"])
        .status()
        .expect("run");
    let mut client = common::connect(&daemon.socket).await;
    let second = client
        .remove_sandbox(RemoveSandboxRequest { id: name.into() })
        .await;

    // Assert
    assert!(status.success());
    assert_eq!(second.err().map(|s| s.code()), Some(Code::NotFound));
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_exec_i_with_stdin_bytes_when_stdin_closes_then_the_stream_ends_cleanly() {
    // Arrange
    let daemon = Daemon::spawn();
    let id = create_sandbox(&daemon);

    // Act
    let mut cmd = daemon.cli();
    cmd.args(["exec", "-i", &id, "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"a\0b")
        .expect("write");
    let out = child.wait_with_output().expect("wait");

    // Assert
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("stub: cat"));
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_a_sandbox_that_printed_when_logs_runs_without_follow_then_it_prints_and_exits() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut cmd = daemon.cli();
    cmd.args(["run", "-d", "--name", "plain-logs", "alpine", "echo"]);
    assert!(cmd.output().expect("run -d").status.success());

    // Act
    let text = logs_until(&daemon, &["logs", "plain-logs"], "stub: echo");

    // Assert
    assert!(text.contains("stub: echo"), "got {text:?}");
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_a_finished_process_when_logs_f_runs_then_it_prints_the_output_and_exits() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut cmd = daemon.cli();
    cmd.args(["run", "-d", "--name", "follow-logs", "alpine", "echo"]);
    assert!(cmd.output().expect("run -d").status.success());

    // Act
    let text = logs_until(&daemon, &["logs", "-f", "follow-logs"], "stub: echo");

    // Assert
    assert!(text.contains("stub: echo"), "got {text:?}");
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_a_named_sandbox_when_plain_exec_runs_then_the_name_is_resolved() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut cmd = daemon.cli();
    cmd.args(["run", "-d", "--name", "by-name", "alpine", "true"]);
    assert!(cmd.output().expect("run -d").status.success());

    // Act
    let mut cmd = daemon.cli();
    let assertion = cmd.args(["exec", "by-name", "echo"]).assert();

    // Assert
    assertion.success().stdout(predicate::str::contains("pid:"));
}

#[cfg(not(feature = "krunvm"))]
#[test]
fn given_exec_it_inside_a_pty_when_the_command_ends_then_the_cli_exits_with_its_code() {
    pty_exec_exits_with(0);
}

#[cfg(feature = "krunvm")]
#[test]
fn given_a_real_guest_when_exec_it_runs_exit_4_then_the_cli_exits_4() {
    if std::env::var("WARD_E2E_VM").as_deref() != Ok("1") {
        eprintln!("skipped: set WARD_E2E_VM=1 to run real-VM tests");
        return;
    }
    pty_exec_exits_with(4);
}

/// Runs `ward exec -it SB sh -c 'exit N'` with a pty as the CLI's terminal.
fn pty_exec_exits_with(code: i32) {
    use rustix::fs::{Mode, OFlags, open};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    // Arrange
    let daemon = if cfg!(feature = "krunvm") {
        Daemon::spawn_vm()
    } else {
        Daemon::spawn()
    };
    let id = create_sandbox(&daemon);
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let slave_path = ptsname(&master, Vec::new()).expect("ptsname");
    let open_slave = || {
        let slave = open(
            slave_path.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        );
        Stdio::from(slave.expect("open slave"))
    };

    // The terminal flushes its output queue on every mode change, so
    // something must read the master side or the CLI blocks.
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; DRAIN_BUFFER_BYTES];
        while rustix::io::read(&master, &mut buf).is_ok_and(|n| n > 0) {}
    });

    // Act
    let status = daemon
        .cli()
        .args(["exec", "-it", &id, "sh", "-c", &format!("exit {code}")])
        .stdin(open_slave())
        .stdout(open_slave())
        .stderr(open_slave())
        .status()
        .expect("exec -it");

    // Assert
    assert_eq!(status.code(), Some(code));
    drain.join().expect("drain thread");
}

#[cfg(feature = "krunvm")]
#[test]
fn given_a_real_guest_when_run_exits_3_then_the_cli_exits_3() {
    if std::env::var("WARD_E2E_VM").as_deref() != Ok("1") {
        eprintln!("skipped: set WARD_E2E_VM=1 to run real-VM tests");
        return;
    }
    let daemon = Daemon::spawn_vm();

    let status = daemon
        .cli()
        .args(["run", &image(), "sh", "-c", "exit 3"])
        .status()
        .expect("run");

    assert_eq!(status.code(), Some(3));
}
