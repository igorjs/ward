// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E test for graceful shutdown on SIGTERM (igorjs/ward#16).
//!
//! Scenario: a running `wardd` with at least one sandbox should respond
//! to SIGTERM by draining RPCs, tearing down every sandbox, and exiting
//! with status 0 within `WARD_SHUTDOWN_TIMEOUT_SECS` (here forced to 5s
//! to keep the test fast).
//!
//! Why a dedicated harness instead of common::Daemon:
//! `common::Daemon::Drop` sends SIGKILL, which masks shutdown bugs. This
//! test wants the opposite: let the daemon observe SIGTERM, run its
//! drain path, and exit cleanly so we can assert on the exit status.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Send SIGTERM to a child by pid. Wraps the rustix call so the test
/// body stays focused on assertions.
fn sigterm(pid: u32) {
    use rustix::process::{Pid, Signal};
    // SAFETY: pid is the OS pid we just spawned; conversion is total.
    let pid = Pid::from_raw(pid as i32).expect("pid > 0");
    rustix::process::kill_process(pid, Signal::TERM).expect("kill SIGTERM");
}

#[test]
fn given_running_daemon_with_sandbox_when_sigterm_then_clean_exit() {
    // Arrange: spawn wardd with isolated socket + data dir and a tight
    // shutdown timeout so a regression in the drain path surfaces fast.
    let data_dir = tempfile::tempdir().expect("tmpdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = Command::cargo_bin("wardd")
        .expect("locate wardd binary")
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_SHUTDOWN_TIMEOUT_SECS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wardd");

    // Wait for the socket to appear so we know the daemon is accepting RPCs.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        if Instant::now() > deadline {
            let _ = wardd.kill();
            let _ = wardd.wait();
            panic!("wardd did not bind {} within 5s", socket.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Create a sandbox so the teardown loop has something to drain.
    // Default backend is the stub when --features krunvm is off; the
    // stub still goes through SandboxManager + Backend::create so the
    // shutdown teardown path is exercised end-to-end.
    let create = Command::cargo_bin("ward")
        .expect("locate ward binary")
        .env("WARD_SOCKET", &socket)
        .args(["create", "alpine:latest"])
        .output()
        .expect("ward create");
    assert!(
        create.status.success(),
        "ward create failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&create.stdout),
        String::from_utf8_lossy(&create.stderr)
    );

    // Act: send SIGTERM and wait for the daemon to exit on its own. If
    // graceful shutdown is broken the daemon hangs and `try_wait` keeps
    // returning Ok(None) until the deadline elapses; we surface that as
    // a clear panic instead of a flaky test that times out at the
    // cargo level.
    let pid = wardd.id();
    sigterm(pid);

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match wardd.try_wait().expect("try_wait") {
            Some(s) => break s,
            None => {
                if Instant::now() > deadline {
                    let _ = wardd.kill();
                    let _ = wardd.wait();
                    panic!(
                        "wardd did not exit within 10s of SIGTERM; \
                         graceful-shutdown drain is hung or signal handler \
                         is not installed"
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // Assert: exit code is 0 (clean drain). A non-zero exit would mean
    // either the drain timeout fired (WARD_SHUTDOWN_TIMEOUT_SECS=5 is
    // generous for the stub backend, which removes sandboxes in <1ms
    // each) or the daemon hit an error during teardown.
    assert!(
        status.success(),
        "wardd exited with non-zero status after SIGTERM: {status:?}"
    );

    // Socket file should be gone (cleaned up on exit). Lingering socket
    // files break the next daemon startup with EADDRINUSE.
    assert!(
        !socket.exists(),
        "socket file still present after clean exit: {}",
        socket.display()
    );
}

#[test]
fn given_running_daemon_without_sandboxes_when_sigint_then_exits_quickly() {
    // Arrange: daemon with no sandboxes; the teardown loop is empty and
    // the daemon should exit nearly instantly. SIGINT is the path a
    // developer hits in the foreground (Ctrl-C), so cover it explicitly
    // alongside SIGTERM (the systemd / launchd path).
    let data_dir = tempfile::tempdir().expect("tmpdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = Command::cargo_bin("wardd")
        .expect("locate wardd binary")
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wardd");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        if Instant::now() > deadline {
            let _ = wardd.kill();
            let _ = wardd.wait();
            panic!("wardd did not bind socket within 5s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Act: send SIGINT.
    use rustix::process::{Pid, Signal};
    let pid = Pid::from_raw(wardd.id() as i32).expect("pid > 0");
    rustix::process::kill_process(pid, Signal::INT).expect("kill SIGINT");

    // Assert: exits within 3s. The no-sandbox path is just "tonic
    // drains in-flight (zero) + teardown loop iterates zero items +
    // remove socket file"; 3s is comfortable headroom.
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match wardd.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() > deadline => {
                let _ = wardd.kill();
                let _ = wardd.wait();
                panic!("wardd did not exit within 3s of SIGINT");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert!(
        status.success(),
        "wardd exited non-zero on SIGINT: {status:?}"
    );
}
