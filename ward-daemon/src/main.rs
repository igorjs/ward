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
use ward_core::validate::MAX_PUBLISH_PAYLOAD_BYTES;
use ward_core::volume::VolumeManager;

// gRPC transport caps. Decode > broker payload cap so an oversize
// publish surfaces as `InvalidArgument` from the validator, not as a
// confusing transport error from the decoder. Encode is double the
// decode ceiling to accommodate response framing for max-sized RPCs.
const MAX_DECODE_BYTES: usize = 1_572_864; // 1.5 MiB
const MAX_ENCODE_BYTES: usize = 2_097_152; // 2 MiB

// HTTP/2 spec recommends 100 concurrent streams per connection as a
// defensive ceiling. Per-conn limit + the global ConcurrencyLimitLayer
// together cap server-side resource use under load.
const MAX_STREAMS_PER_CONN: usize = 100;

// Compile-time invariant: decode cap must exceed broker publish cap,
// otherwise a max-payload PublishRequest gets rejected as transport-
// level FRAME_SIZE_ERROR before the validator's `InvalidArgument`
// branch can ever fire. Promoting both to consts lets the relationship
// be enforced at build time rather than re-discovered in production.
const _: () = assert!(
    MAX_DECODE_BYTES > MAX_PUBLISH_PAYLOAD_BYTES,
    "MAX_DECODE_BYTES must exceed MAX_PUBLISH_PAYLOAD_BYTES so oversize publishes \
     surface as InvalidArgument from validate::publish_payload, not as a tonic \
     transport error."
);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // SEC-002: clamp the process umask BEFORE binding the Unix socket or
    // creating data directories. Without this the socket file and the
    // data tree inherit whatever umask the operator's shell set
    // (usually 022; under root with umask 0, world-writable). With
    // 0o077 every subsequent file/dir is born owner-only, closing the
    // bind-then-chmod window that the subsequent explicit chmod on
    // the socket file would otherwise leave open.
    #[cfg(unix)]
    {
        rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
    }

    let cfg = Config::from_env();

    // Initialise structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .init();

    // SEC-019: WARD_REGISTRY_ALLOWLIST="" is operator-reachable (e.g.
    // a malformed unit file `Environment=WARD_REGISTRY_ALLOWLIST=` with
    // no value). The image-pull path treats empty-after-trim as "unset"
    // (allow any registry), which is the safer of the two possible
    // semantics, but operators who set the var expecting an allowlist
    // deserve a loud signal that they got the opposite. Empty / unset
    // distinguishes here at startup so the message is delivered once,
    // not on every pull.
    if let Ok(raw) = std::env::var("WARD_REGISTRY_ALLOWLIST")
        && raw.trim().is_empty()
        && !raw.is_empty()
    {
        tracing::warn!(
            "WARD_REGISTRY_ALLOWLIST is set to an empty / whitespace-only value; \
             treating as UNSET (all registries allowed). Unset the variable to \
             silence this warning, or set it to a comma-separated list of \
             registries (e.g. \"docker.io,ghcr.io\") to enforce an allowlist."
        );
    }

    // Loud warning when WARD_METRICS_ADDR is set but unparseable.
    // Config::from_values silently falls back to None on parse failure
    // (so a typo cannot crash the daemon at startup) but operators
    // who set the var expecting /metrics to appear deserve to see why
    // it did not. Empty / unset values stay silent (the documented
    // opt-out).
    if cfg.metrics_addr.is_none()
        && let Ok(raw) = std::env::var("WARD_METRICS_ADDR")
        && !raw.trim().is_empty()
    {
        tracing::warn!(
            value = %raw,
            "WARD_METRICS_ADDR is set but did not parse as a SocketAddr (expected host:port); Prometheus exporter NOT installed"
        );
    }

    // Install the Prometheus exporter when WARD_METRICS_ADDR is set.
    // Without an exporter, metrics::counter!() / histogram!() are no-ops
    // (the recorder facade discards them), so call sites elsewhere
    // can instrument unconditionally without paying for it when
    // metrics aren't scraped.
    if let Some(addr) = cfg.metrics_addr {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .map_err(|e| format!("install prometheus exporter on {addr}: {e}"))?;
        tracing::info!(metrics = %addr, "metrics scrape endpoint listening at /metrics");
    }

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
        cfg.allow_host_mounts,
    ));
    let volume_mgr = Arc::new(VolumeManager::new(cfg.data_dir.clone(), cfg.max_volumes));

    let grpc_service = WardGrpcServer::new(Arc::clone(&sandbox_mgr), Arc::clone(&volume_mgr));

    // SEC-009: cap per-message size on both decode and encode so a
    // hostile (or buggy) client can't force the daemon to allocate
    // multi-MiB buffers per request. Tonic defaults to 4 MiB decode /
    // unbounded encode; we tighten both. See the file-level const
    // block + compile-time assert for the decode-vs-broker-cap
    // invariant.
    let ward_service = WardServer::new(grpc_service)
        .max_decoding_message_size(MAX_DECODE_BYTES)
        .max_encoding_message_size(MAX_ENCODE_BYTES);

    // Bind the Unix domain socket.
    // SEC-002: tokio::UnixListener::bind creates the socket with the process
    // umask, which on permissive defaults (or under root) leaves the socket
    // group/world-connectable. ADR-004 promises 0600 — enforce it here so
    // only the daemon's owner can connect.
    let uds = tokio::net::UnixListener::bind(&cfg.socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
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

    // SEC-009 + SEC-015: cap concurrency at two levels.
    // - concurrency_limit_per_connection caps in-flight streams per
    //   client connection. Matches the HTTP/2 spec recommendation.
    // - tower::ConcurrencyLimitLayer caps in-flight RPCs across ALL
    //   connections. Set to 2x max_sandboxes so the daemon can
    //   service a peak operation per sandbox (e.g. one stream_output
    //   each) without piling up unbounded tokio::spawn tasks.
    //
    // `.max(1)` before the saturating multiply protects against a
    // `WARD_MAX_SANDBOXES=0` operator footgun: `ConcurrencyLimitLayer::new(0)`
    // is a semaphore with zero permits, so every RPC parks forever
    // and the daemon silently wedges. Flooring at 1 keeps the cap
    // semantically "at least one RPC may run" no matter what the
    // operator configured.
    let max_total_streams = compute_max_total_streams(cfg.max_sandboxes);
    Server::builder()
        .concurrency_limit_per_connection(MAX_STREAMS_PER_CONN)
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_total_streams))
        .add_service(ward_service)
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

/// Pure-function form of the global concurrency cap so unit tests can
/// pin the floor and ceiling behaviours without standing up a tonic
/// server. Floors at 1 so an operator-supplied `WARD_MAX_SANDBOXES=0`
/// cannot wedge the daemon by producing a zero-permit semaphore. Caps
/// at 65_536 so an arbitrarily large operator value cannot effectively
/// disable the cap; that ceiling is comfortable above any realistic
/// host's capacity.
fn compute_max_total_streams(max_sandboxes: usize) -> usize {
    const MAX_SANITY_CEILING: usize = 65_536;
    max_sandboxes
        .max(1)
        .saturating_mul(2)
        .min(MAX_SANITY_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_max_sandboxes_zero_when_compute_then_floors_at_two() {
        // Regression guard: max_sandboxes=0 must not produce a zero-permit
        // semaphore. Floored to 1, then *2 = 2 permits, so even a
        // misconfigured daemon can still service a couple of RPCs.
        assert_eq!(compute_max_total_streams(0), 2);
    }

    #[test]
    fn given_max_sandboxes_typical_when_compute_then_doubles() {
        // Default WARD_MAX_SANDBOXES is 256; 2x is the documented headroom.
        assert_eq!(compute_max_total_streams(256), 512);
    }

    #[test]
    fn given_max_sandboxes_huge_when_compute_then_caps_at_sanity_ceiling() {
        // An operator typing 1_000_000 by accident must not silently
        // disable the cap. 65_536 is the documented ceiling.
        assert_eq!(compute_max_total_streams(1_000_000), 65_536);
        assert_eq!(compute_max_total_streams(usize::MAX), 65_536);
    }
}
