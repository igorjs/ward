// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E test harness: spawn `wardd` as a subprocess and tear it down cleanly.
//!
//! E2E tests simulate a real user: a daemon process is running, and the
//! user types `ward <command>` at a shell. The harness:
//!
//!   1. Allocates a per-test temp dir (auto-cleaned on Drop).
//!   2. Computes a per-test Unix socket path inside that dir.
//!   3. Spawns the compiled `wardd` binary with WARD_SOCKET / WARD_DATA_DIR
//!      pointed at the per-test paths.
//!   4. Waits for the socket file to appear (daemon is ready).
//!   5. Returns a `Daemon` guard whose `Drop` impl SIGKILLs the subprocess.
//!
//! Per-test isolation means tests can run in parallel without colliding on
//! sockets or sandbox state.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// RAII guard around a running `wardd` subprocess. SIGKILLs the daemon on
/// drop so failed tests do not leak zombies.
pub struct Daemon {
    /// Per-test temp directory; deleted recursively when Daemon is dropped.
    _data_dir: TempDir,
    /// Absolute path to the daemon's Unix socket.
    pub socket: PathBuf,
    /// Subprocess handle. Wrapped in Option so Drop can take ownership.
    child: Option<Child>,
}

impl Daemon {
    /// Spawn `wardd` with isolated socket + data dirs and wait until it
    /// is accepting connections. Panics with a helpful message on timeout.
    pub fn spawn() -> Self {
        let data_dir = tempfile::tempdir().expect("create temp dir");
        let socket = data_dir.path().join("ward.sock");

        // Use assert_cmd to resolve the workspace's built wardd binary.
        // This works regardless of debug/release profile.
        let mut cmd = Command::cargo_bin("wardd").expect("locate wardd binary");
        cmd.env("WARD_SOCKET", &socket)
            .env("WARD_DATA_DIR", data_dir.path())
            .env("WARD_LOG_LEVEL", "warn") // quiet startup logs in test output
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().expect("spawn wardd");

        // Poll for socket existence. Daemon takes ~50ms to bind; allow up
        // to 5s to handle slow CI runners. If the daemon panics during
        // startup, the socket never appears and we surface that as a clear
        // panic instead of a flaky ECONNREFUSED later.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                return Self {
                    _data_dir: data_dir,
                    socket,
                    child: Some(child),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Reap the zombie before panicking — clippy's zombie_processes
        // lint flags this and rightly so: a failed test must not leak
        // background processes that outlive cargo.
        let _ = child.kill();
        let _ = child.wait();
        panic!("wardd did not bind socket within 5s: {}", socket.display());
    }

    /// Build a `Command` for the `ward` CLI pre-configured to talk to this
    /// daemon. Tests add their own subcommand and arguments.
    pub fn cli(&self) -> Command {
        let mut cmd = Command::cargo_bin("ward").expect("locate ward binary");
        cmd.env("WARD_SOCKET", &self.socket);
        cmd
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGKILL — we do not need a clean shutdown for tests, and a
            // hung daemon must not block test teardown.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
