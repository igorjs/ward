// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration test for gvproxy socket spawn path.
//!
//! Gated on `which gvproxy` succeeding — skipped with a clear message otherwise.
//! Does NOT require libkrun or the `krunvm` feature.

#[cfg(feature = "gvproxy")]
mod gvproxy_spawn_tests {
    use ward_net::AttachOptions;
    use ward_net::gvproxy::{GvproxyHandle, spawn_for_sandbox};

    fn gvproxy_available() -> bool {
        which::which("gvproxy").is_ok()
    }

    #[tokio::test]
    async fn given_gvproxy_available_when_spawn_then_returns_valid_handle_and_socket() {
        if !gvproxy_available() {
            eprintln!("SKIP: gvproxy not found on PATH — install gvproxy to run this test");
            return;
        }

        let opts = AttachOptions::default();
        let mut handle: GvproxyHandle = spawn_for_sandbox("test-gv-spawn", &opts)
            .await
            .expect("spawn_for_sandbox should succeed when gvproxy is on PATH");

        // The socket path must be set.
        assert!(
            !handle.socket_path.as_os_str().is_empty(),
            "socket_path should be non-empty"
        );

        // Give gvproxy a moment to bind the socket.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify the socket exists on disk after spawn.
        assert!(
            handle.socket_path.exists(),
            "gvproxy socket {:?} should exist after spawn",
            handle.socket_path
        );

        // Clean kill.
        handle.kill().await.expect("kill should be idempotent");
    }

    #[tokio::test]
    async fn given_gvproxy_spawned_when_kill_twice_then_idempotent() {
        if !gvproxy_available() {
            eprintln!("SKIP: gvproxy not found on PATH — install gvproxy to run this test");
            return;
        }

        let opts = AttachOptions::default();
        let mut handle = spawn_for_sandbox("test-gv-idempotent", &opts)
            .await
            .expect("spawn");

        handle.kill().await.expect("first kill");
        handle
            .kill()
            .await
            .expect("second kill must not panic or error");
    }

    #[tokio::test]
    async fn given_gvproxy_available_when_spawn_then_socket_path_contains_sandbox_id() {
        if !gvproxy_available() {
            eprintln!("SKIP: gvproxy not found on PATH — install gvproxy to run this test");
            return;
        }

        let opts = AttachOptions::default();
        let mut handle = spawn_for_sandbox("test-gv-id-check", &opts)
            .await
            .expect("spawn");

        let path_str = handle.socket_path.to_string_lossy();
        assert!(
            path_str.contains("test-gv-id-check"),
            "socket path {path_str:?} should contain the sandbox id"
        );
        assert!(
            path_str.ends_with(".sock"),
            "socket path {path_str:?} should end with .sock"
        );

        handle.kill().await.ok();
    }
}
