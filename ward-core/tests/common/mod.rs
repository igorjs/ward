// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Shared test harness for gRPC integration tests.
//!
//! Each integration test file (`tests/*.rs`) is compiled as its own crate by
//! Cargo, so they cannot share code via regular `mod` declarations. The
//! convention is a `common/` subdirectory containing a `mod.rs`, included by
//! each test file with `mod common;`. Files under subdirectories are not
//! auto-discovered as test crates, so this avoids the "phantom test crate"
//! warning the runner would otherwise emit.
//!
//! The harness spins up a real `tonic` server on a kernel-assigned port and
//! returns a connected client. Going through the wire format exercises the
//! same serialisation / deserialisation path real SDK clients use, which is
//! the whole point of an integration test — bypassing it via direct trait
//! calls misses entire classes of bugs (proto field reuse, wrong status
//! codes, encoding errors).

use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Endpoint, Server};

use ward_core::backend::krunvm::KrunvmBackend;
use ward_core::comms::Broker;
use ward_core::grpc::WardGrpcServer;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::ward_server::WardServer;
use ward_core::sandbox::SandboxManager;
use ward_core::volume::VolumeManager;

/// Spin up a Ward gRPC server bound to a kernel-assigned loopback port and
/// return a connected client.
///
/// Each call creates fresh manager instances so tests don't share state.
/// The server task is detached: it terminates when the test process exits
/// or the listener is dropped.
///
/// # Configuration
///
/// - `data_dir`: a temporary directory unique to this test run. Created
///   with `tempfile` so it self-cleans on drop.
/// - `max_sandboxes` / `max_volumes`: small caps (4) so capacity-limit
///   tests can hit them without creating hundreds of sandboxes.
///
/// # Returns
///
/// A `WardClient<Channel>` already connected to the server, ready to call
/// any RPC defined on the `Ward` service.
pub async fn test_server() -> WardClient<Channel> {
    // 1) Build domain managers. The KrunvmBackend without the "krunvm"
    //    feature is a stub: it tracks state but doesn't touch libkrun. That
    //    is exactly what we want for boundary-validation tests.
    let data_dir = std::env::temp_dir().join(format!("ward-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("create temp data dir");

    let backend = Arc::new(KrunvmBackend::new(data_dir.clone()));
    let broker = Arc::new(Broker::new());
    let sandbox_mgr = Arc::new(SandboxManager::new(
        Arc::clone(&backend),
        Arc::clone(&broker),
        4,
    ));
    let volume_mgr = Arc::new(VolumeManager::new(data_dir, 4));
    let service = WardGrpcServer::new(sandbox_mgr, volume_mgr);

    // 2) Bind to loopback on port 0 — the kernel picks a free port for us.
    //    This avoids hard-coded port collisions when tests run in parallel.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let incoming = TcpListenerStream::new(listener);

    // 3) Spawn the server. The handle is dropped intentionally; the task
    //    keeps running until either the listener errors or the test exits.
    tokio::spawn(async move {
        Server::builder()
            .add_service(WardServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .expect("server")
    });

    // 4) Connect a client. `Endpoint::connect()` doesn't retry, so we
    //    spin briefly to give the server a moment to start accepting.
    let endpoint = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(1));

    for attempt in 0..10 {
        match endpoint.connect().await {
            Ok(channel) => return WardClient::new(channel),
            Err(_) if attempt < 9 => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => panic!("connect to test server: {e}"),
        }
    }
    unreachable!()
}
