// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Smoke test: prove the test harness wires up.
//!
//! If this passes, every other integration test in this crate has a working
//! foundation: tonic server on a real socket, real client connection, real
//! RPC dispatch. Keeping it as a separate file means a harness regression
//! shows up as a single failing test rather than a wave of failures across
//! every other integration file.

mod common;

#[tokio::test]
async fn server_responds_to_get_info() {
    let mut client = common::test_server().await;

    // Use a no-arg RPC — GetInfo always succeeds and never depends on
    // sandbox state, so the only way this fails is a broken harness.
    let resp = client.get_info(()).await.expect("get_info should succeed");

    let info = resp.into_inner();
    assert!(!info.version.is_empty(), "version must be populated");
    assert!(!info.platform.is_empty(), "platform must be populated");
    assert_eq!(info.backend, "krunvm");
}
