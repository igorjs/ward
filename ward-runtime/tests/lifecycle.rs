// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Lifecycle integration test for `ward-runtime`.
//!
//! Unit tests in `lib.rs` cover the builder + from_config constructors;
//! this file proves the embedded path actually drives a sandbox through
//! create → list → remove without the daemon shim. Stub backend (default
//! features, no `krunvm` feature) so the test runs on any platform
//! without libkrun.
//!
//! The test mirrors what a Rust application calling
//! `Runtime::builder()...create()` and then driving the sandbox manager
//! in-process would do — i.e. the use case ADR-016 introduces.

use std::collections::HashMap;

use ward_core::pb;
use ward_runtime::Runtime;

/// Helper: build a `CreateSandboxRequest` with the minimum fields the
/// manager requires. Mirrors `ward-cli/src/main.rs::Commands::Create`
/// translation logic so the embedded path exercises the same shape the
/// gRPC path does.
fn create_request(image: &str) -> pb::CreateSandboxRequest {
    pb::CreateSandboxRequest {
        image: image.into(),
        resources: Some(pb::ResourceLimits {
            cpus: 1,
            memory_mb: 256,
            pids_max: 0,
            timeout_seconds: 0,
        }),
        env: HashMap::new(),
        comms: Some(pb::CommunicationPolicy {
            mode: pb::CommunicationMode::Deny as i32,
            group: String::new(),
        }),
        egress: Some(pb::EgressPolicy {
            mode: pb::EgressMode::Deny as i32,
            domains: Vec::new(),
        }),
        mounts: Vec::new(),
        volume_ids: Vec::new(),
        from_snapshot: String::new(),
    }
}

#[tokio::test]
async fn given_runtime_when_create_then_list_then_remove_then_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // ARRANGE — boot the embedded runtime exactly like a Rust app would.
    let runtime = Runtime::builder()
        .data_dir(tmp.path())
        .max_sandboxes(4)
        .max_volumes(4)
        .build()
        .await
        .expect("runtime builds with stub backend");

    let mgr = runtime.sandbox_manager();

    // ACT 1 — create.
    let info = mgr
        .create(create_request("alpine"))
        .await
        .expect("create succeeds on stub backend");
    assert!(!info.id.is_empty(), "manager should assign a non-empty id");
    assert_eq!(info.image, "alpine");

    // ACT 2 — list and find what we created.
    let infos = mgr.list().await.expect("list succeeds");
    assert!(
        infos.iter().any(|s| s.id == info.id),
        "created sandbox must appear in list: {infos:?}"
    );

    // ACT 3 — remove.
    mgr.remove(&info.id).await.expect("remove succeeds");

    // ASSERT — list is back to empty.
    let infos = mgr.list().await.expect("list after remove");
    assert!(
        infos.iter().all(|s| s.id != info.id),
        "removed sandbox must not appear in list: {infos:?}"
    );
}

#[tokio::test]
async fn given_runtime_when_cap_reached_then_next_create_errors() {
    // Regression: max_sandboxes is the runtime's hard cap. The embedded
    // path must enforce it, not just the daemon path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = Runtime::builder()
        .data_dir(tmp.path())
        .max_sandboxes(2)
        .build()
        .await
        .expect("runtime builds");
    let mgr = runtime.sandbox_manager();

    // Fill the cap.
    mgr.create(create_request("alpine")).await.expect("1st");
    mgr.create(create_request("alpine")).await.expect("2nd");

    // 3rd should fail with a cap error.
    let err = mgr
        .create(create_request("alpine"))
        .await
        .expect_err("3rd create must fail because cap is 2");
    // ApiError doesn't expose a public discriminant; assert on the
    // Display surface (the message users actually see).
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("limit") || msg.contains("max") || msg.contains("cap"),
        "expected cap-related error, got: {err}"
    );
}

#[tokio::test]
async fn given_runtime_when_clone_then_managers_shared() {
    // Regression: Runtime is `Clone`. Two clones must address the same
    // underlying state — embedded users that fork the runtime into
    // multiple workers rely on this.
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = Runtime::builder()
        .data_dir(tmp.path())
        .build()
        .await
        .expect("runtime");

    let a = runtime.clone();
    let b = runtime.clone();

    let info = a
        .sandbox_manager()
        .create(create_request("alpine"))
        .await
        .expect("a creates");

    // b sees what a created.
    let seen = b
        .sandbox_manager()
        .list()
        .await
        .expect("b lists")
        .into_iter()
        .any(|s| s.id == info.id);
    assert!(seen, "cloned Runtime must share manager state");
}
