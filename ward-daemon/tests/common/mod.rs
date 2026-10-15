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
    /// Daemon output; printed when the test that owns the daemon panics.
    log: PathBuf,
}

impl Daemon {
    /// Spawn `wardd` with isolated socket + data dirs and wait until it
    /// is accepting connections. Panics with a helpful message on timeout.
    #[allow(dead_code)]
    pub fn spawn() -> Self {
        Self::spawn_inner(false)
    }

    /// Like [`Daemon::spawn`] but images are really pulled and sandboxes
    /// really boot as microVMs. Needs a `krunvm` build and a hypervisor.
    #[allow(dead_code)]
    pub fn spawn_vm() -> Self {
        Self::spawn_inner(true)
    }

    /// Process id of the daemon.
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("daemon running").id()
    }

    /// SIGKILL the daemon without cleanup and reap it.
    #[allow(dead_code)]
    pub fn kill_hard(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn spawn_inner(real_vms: bool) -> Self {
        let data_dir = tempfile::tempdir().expect("create temp dir");
        let socket = data_dir.path().join("ward.sock");
        let log = data_dir.path().join("wardd.log");
        let daemon_log = std::fs::File::create(&log).expect("create daemon log");

        // Use assert_cmd to resolve the workspace's built wardd binary.
        // This works regardless of debug/release profile.
        let mut cmd = Command::cargo_bin("wardd").expect("locate wardd binary");
        cmd.env("WARD_SOCKET", &socket)
            .env("WARD_DATA_DIR", data_dir.path())
            .env("WARD_LOG_LEVEL", "info")
            // WARD_OCI_OFFLINE=1: skip real registry pulls in e2e tests.
            // CI runs with egress-policy:block (step-security/harden-runner)
            // and docker.io is not in the allowed-endpoints list. Setting this
            // makes OciPuller materialise a minimal synthetic rootfs instead of
            // hitting the network, keeping e2e tests hermetic on any host.
            .stdin(Stdio::null())
            .stdout(Stdio::from(daemon_log.try_clone().expect("clone log")))
            .stderr(Stdio::from(daemon_log));

        if real_vms {
            seed_image_cache(data_dir.path());
            ensure_hypervisor_entitlement(cmd.get_program());
            // One extraction of the embedded libkrun shared by all tests.
            cmd.env("WARD_RUNTIME_DIR", shared_runtime_dir());
            // Mount tests share directories from the test's temp dir.
            cmd.env("WARD_ALLOW_HOST_MOUNTS", "1");
        } else {
            cmd.env("WARD_OCI_OFFLINE", "1");
        }

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
                    log,
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
        if std::thread::panicking()
            && let Ok(log) = std::fs::read_to_string(&self.log)
        {
            eprintln!("---- wardd log ----\n{log}");
        }
        if let Some(mut child) = self.child.take() {
            // SIGKILL — we do not need a clean shutdown for tests, and a
            // hung daemon must not block test teardown.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Directory that holds the extracted libkrun for all VM tests in a run.
#[allow(dead_code)]
fn shared_runtime_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ward-e2e-runtime");
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

/// macOS only: Hypervisor.framework refuses processes without the
/// `com.apple.security.hypervisor` entitlement, so sign the freshly built
/// daemon once per test run. A no-op elsewhere.
#[allow(dead_code)]
fn ensure_hypervisor_entitlement(program: &std::ffi::OsStr) {
    static SIGNED: std::sync::Once = std::sync::Once::new();
    SIGNED.call_once(|| {
        if !cfg!(target_os = "macos") {
            return;
        }
        let entitlements = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("entitlements.plist");
        let status = Command::new("codesign")
            .args(["--force", "--sign", "-", "--entitlements"])
            .arg(entitlements)
            .arg(program)
            .status()
            .expect("run codesign");
        assert!(status.success(), "codesign failed for {program:?}");
    });
}

/// Pull the test image once per machine, then give every test daemon a copy
/// of that cache. Without it each test would hit the registry and anonymous
/// pulls get throttled.
#[allow(dead_code)]
fn seed_image_cache(data_dir: &std::path::Path) {
    static PRIMED: std::sync::Once = std::sync::Once::new();
    let template = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ward-e2e-image-cache");
    let image = std::env::var("WARD_E2E_IMAGE").unwrap_or_else(|_| "alpine:3.20".to_string());
    PRIMED.call_once(|| {
        let marker = template.join(".primed");
        let wanted = format!("{image}\n");
        if std::fs::read_to_string(&marker).is_ok_and(|m| m == wanted) {
            return;
        }
        let _ = std::fs::remove_dir_all(&template);
        let dir = tempfile::tempdir().expect("prime dir");
        let socket = dir.path().join("ward.sock");
        let prime_log_path = dir.path().join("wardd.log");
        let prime_log = std::fs::File::create(&prime_log_path).expect("create log");
        let mut cmd = Command::cargo_bin("wardd").expect("locate wardd");
        cmd.env("WARD_SOCKET", &socket)
            .env("WARD_DATA_DIR", dir.path())
            .env("WARD_RUNTIME_DIR", shared_runtime_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::from(prime_log.try_clone().expect("clone log")))
            .stderr(Stdio::from(prime_log));
        ensure_hypervisor_entitlement(cmd.get_program());
        let mut daemon = cmd.spawn().expect("spawn priming wardd");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = Command::cargo_bin("ward")
            .expect("locate ward")
            .env("WARD_SOCKET", &socket)
            .args(["create", &image])
            .output()
            .expect("prime pull");
        let _ = daemon.kill();
        let _ = daemon.wait();
        assert!(
            out.status.success(),
            "priming pull of {image} failed: {}\n---- wardd log ----\n{}",
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&prime_log_path).unwrap_or_default()
        );
        copy_tree(&dir.path().join("cache"), &template.join("cache"));
        std::fs::write(&marker, wanted).expect("write marker");
    });
    copy_tree(&template.join("cache"), &data_dir.join("cache"));
}

/// Copy a directory tree, hard-linking files when the filesystem allows.
#[allow(dead_code)]
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = to.join(entry.file_name());
        let kind = entry.file_type().expect("file type");
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if kind.is_symlink() {
            let link = std::fs::read_link(entry.path()).expect("read link");
            std::os::unix::fs::symlink(link, target).expect("symlink");
        } else if std::fs::hard_link(entry.path(), &target).is_err() {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}
