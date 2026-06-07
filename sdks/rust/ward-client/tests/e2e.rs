// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! End-to-end integration test for the Rust SDK.
//!
//! Spawns a real `wardd` subprocess against a per-test data dir /
//! Unix socket, connects via [`WardClient::connect_socket`], and
//! drives the full sandbox lifecycle (create → list → run →
//! kill → remove) plus a few error paths.
//!
//! All tests run against the **stub backend** (default features, no
//! `krunvm`) so they need no libkrun and no kernel privileges. The
//! point is to prove the SDK's wire-level behaviour against the same
//! daemon binary users will deploy — not to exercise real microVMs
//! (those need `--features krunvm` + libkrun, see docs/platforms.md).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use ward_client::{CreateOptions, EgressMode, WardClient, WardError};

/// Resolve the wardd binary path without pulling in ward-daemon as a
/// dev-dep (which would breach the Apache-2.0 ↔ AGPL boundary
/// documented in `Cargo.toml`).
///
/// Order of attempts:
///   1. `CARGO_BIN_EXE_wardd` env var. Cargo sets this automatically
///      for the crate that owns the binary; for cross-crate cases
///      (this one) it's typically unset.
///   2. Workspace fallback: walk `CARGO_MANIFEST_DIR` ancestors looking
///      for `target/{debug,release}/wardd`. Works when the workspace
///      has already been built (i.e. `cargo test --workspace`).
///
/// Returns `None` if neither path resolves — in that case the test
/// skips with a `return` rather than panicking. Coverage CI excludes
/// ward-daemon (so wardd isn't built); skipping is correct.
fn resolve_wardd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_wardd") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        for profile in ["debug", "release"] {
            let candidate = ancestor.join("target").join(profile).join("wardd");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Harness: spawn wardd against an isolated data dir / socket
// ---------------------------------------------------------------------------

/// RAII guard around a running wardd subprocess. SIGKILLs on drop so a
/// failed assertion doesn't leak background processes that outlive
/// `cargo test`.
struct Daemon {
    _data_dir: TempDir,
    pub socket: PathBuf,
    child: Option<Child>,
}

impl Daemon {
    /// Spawn wardd. Returns `None` if the wardd binary isn't present
    /// in the workspace target dir (e.g. under coverage runs that
    /// exclude ward-daemon) — callers should `return` to skip.
    fn try_spawn() -> Option<Self> {
        let wardd = resolve_wardd()?;
        let data_dir = tempfile::tempdir().expect("create temp dir");
        let socket = data_dir.path().join("ward.sock");

        let mut cmd = Command::new(&wardd);
        cmd.env("WARD_SOCKET", &socket)
            .env("WARD_DATA_DIR", data_dir.path())
            .env("WARD_LOG_LEVEL", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().expect("spawn wardd");

        // Poll for socket existence. Daemon binds quickly (~50ms); allow
        // up to 5s for slow CI runners.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket.exists() {
                return Some(Self {
                    _data_dir: data_dir,
                    socket,
                    child: Some(child),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();
        panic!("wardd did not bind socket within 5s: {}", socket.display());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn connect(d: &Daemon) -> WardClient {
    WardClient::connect_socket(&d.socket)
        .await
        .expect("SDK connects to wardd over UDS")
}

fn create_opts(image: &str) -> CreateOptions {
    CreateOptions {
        image: image.into(),
        cpus: 1,
        memory_mb: 256,
        egress: EgressMode::Deny,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn given_sdk_when_create_sandbox_then_returns_running() {
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let sb = client
        .create_sandbox(create_opts("alpine"))
        .await
        .expect("create_sandbox succeeds on stub backend");

    assert!(!sb.id.is_empty(), "id must be assigned: {sb:?}");
    assert_eq!(sb.image, "alpine");
    // Stub backend transitions through Creating → Running synchronously.
    assert!(
        matches!(sb.status.as_str(), "running" | "creating"),
        "unexpected status: {sb:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn given_two_sandboxes_when_list_then_both_appear() {
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let a = client
        .create_sandbox(create_opts("alpine"))
        .await
        .expect("first create");
    let b = client
        .create_sandbox(create_opts("ubuntu"))
        .await
        .expect("second create");

    let listed = client.list_sandboxes().await.expect("list");
    let ids: Vec<_> = listed.iter().map(|s| s.id.clone()).collect();
    assert!(ids.contains(&a.id), "first missing from list: {ids:?}");
    assert!(ids.contains(&b.id), "second missing from list: {ids:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn given_sandbox_when_remove_then_list_no_longer_contains() {
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let sb = client
        .create_sandbox(create_opts("alpine"))
        .await
        .expect("create");
    client
        .remove_sandbox(&sb.id)
        .await
        .expect("remove succeeds");

    let listed = client.list_sandboxes().await.expect("list after remove");
    assert!(
        listed.iter().all(|s| s.id != sb.id),
        "removed sandbox must not appear: {listed:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn given_missing_sandbox_when_get_then_not_found_error() {
    // SDK error mapping: tonic NotFound → WardError::NotFound. Without
    // the per-variant mapping, callers can't distinguish "doesn't
    // exist" from "daemon broke", which agentic clients need.
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let err = client
        .get_sandbox("00000000-0000-0000-0000-000000000000")
        .await
        .expect_err("get for nonexistent id must fail");
    assert!(
        matches!(err, WardError::NotFound(_)),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn given_running_sandbox_when_exec_then_pid_returned() {
    // exec returns the synthetic pid the stub backend assigns. Real
    // libkrun backend will substitute a vsock-routed pid; the SDK
    // shape is the same.
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let sb = client
        .create_sandbox(create_opts("alpine"))
        .await
        .expect("create");

    let pid = client
        .exec(&sb.id, &["echo", "hello"], None)
        .await
        .expect("exec succeeds");
    assert!(!pid.is_empty(), "pid must be non-empty");
}

#[tokio::test(flavor = "current_thread")]
async fn given_running_sandbox_when_run_then_captures_stub_output() {
    // The stub backend emits a scripted stdout line + Exit(0). Run
    // drives exec + stream_output to completion and collects both.
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let mut client = connect(&d).await;

    let sb = client
        .create_sandbox(create_opts("alpine"))
        .await
        .expect("create");

    let result = client.run(&sb.id, &["echo", "hello"]).await.expect("run");
    assert_eq!(
        result.exit_code,
        Some(0),
        "stub backend must emit Exit(0): {result:?}"
    );
    assert!(
        !result.stdout.is_empty(),
        "stub backend must emit at least one stdout line: {result:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn given_endpoint_when_connected_then_unix_uri_displayed() {
    // Diagnostic surface — operators inspecting a logged client must
    // see a URI that points at the right transport.
    let d = match Daemon::try_spawn() {
        Some(d) => d,
        None => {
            eprintln!(
                "ward-client e2e: wardd binary not built; skipping. \
                 Build the workspace first or remove `--exclude ward-daemon`."
            );
            return;
        }
    };
    let client = connect(&d).await;
    let ep = client.endpoint();
    assert!(
        ep.starts_with("unix://"),
        "endpoint must use unix scheme: {ep}"
    );
    assert!(
        ep.contains("ward.sock"),
        "endpoint must reference the socket file: {ep}"
    );
}
