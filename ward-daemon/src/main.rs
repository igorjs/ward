// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward daemon – serves the gRPC API over a Unix socket.

use std::sync::Arc;

use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use ward_core::backend::Backend;
use ward_core::backend::krunvm::KrunvmBackend;
use ward_core::comms::Broker;
use ward_core::config::Config;
use ward_core::grpc::WardGrpcServer;
use ward_core::pb::ward_server::WardServer;
use ward_core::sandbox::SandboxManager;
use ward_core::volume::VolumeManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env();

    // Initialise structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .init();

    tracing::info!(
        socket = %cfg.socket_path.display(),
        data_dir = %cfg.data_dir.display(),
        "ward daemon starting"
    );

    // Ensure required directories exist.
    cfg.ensure_dirs()?;

    // Remove a stale socket file from a previous run.
    if cfg.socket_path.exists() {
        std::fs::remove_file(&cfg.socket_path)?;
    }

    // Build the domain managers. The backend is held as Arc<dyn Backend>
    // so future swaps (Firecracker on Linux, Apple Virtualization.framework
    // on macOS) plug in by changing this line only.
    let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::new(cfg.data_dir.clone()));
    let broker = Arc::new(Broker::new());
    let sandbox_mgr = Arc::new(SandboxManager::new(
        Arc::clone(&backend),
        Arc::clone(&broker),
        cfg.max_sandboxes,
    ));
    let volume_mgr = Arc::new(VolumeManager::new(cfg.data_dir.clone(), cfg.max_volumes));

    let grpc_service = WardGrpcServer::new(Arc::clone(&sandbox_mgr), Arc::clone(&volume_mgr));

    // Bind the Unix domain socket.
    let uds = tokio::net::UnixListener::bind(&cfg.socket_path)?;
    let uds_stream = tokio_stream::wrappers::UnixListenerStream::new(uds);

    tracing::info!(socket = %cfg.socket_path.display(), "listening");

    // Graceful shutdown on SIGTERM / SIGINT.
    let shutdown = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
            }
            _ = sigterm() => {
                tracing::info!("received SIGTERM, shutting down");
            }
        }
    };

    Server::builder()
        .add_service(WardServer::new(grpc_service))
        .serve_with_incoming_shutdown(uds_stream, shutdown)
        .await?;

    // Clean up the socket file on exit.
    let _ = std::fs::remove_file(&cfg.socket_path);
    tracing::info!("ward daemon stopped");

    Ok(())
}

/// Resolves when SIGTERM is received (Unix only).
#[cfg(unix)]
async fn sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut stream = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    stream.recv().await;
}

#[cfg(not(unix))]
async fn sigterm() {
    // On non-Unix platforms, SIGTERM is not available; this future never resolves.
    std::future::pending::<()>().await;
}
