// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward gRPC client connection helper.
//!
//! `tonic` does not natively support Unix sockets; you have to construct an
//! `Endpoint` with a custom connector that hands tonic a `UnixStream`. The
//! URI is a dummy ("http://[::1]") that tonic requires but never actually
//! uses — the connector intercepts the connection and routes it to the
//! socket path we capture in the closure.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use ward_core::pb::ward_client::WardClient;

/// Connect to the ward daemon over the given Unix socket path.
///
/// Returns a typed gRPC client ready to issue RPCs. Fails with a clear
/// error if the socket doesn't exist or the daemon isn't accepting.
pub async fn connect(socket_path: &str) -> Result<WardClient<Channel>> {
    // The URI scheme/host is a placeholder. tonic requires *some* URI even
    // for Unix-socket transports; only the connector below sees real I/O.
    let socket = PathBuf::from(socket_path);

    let channel = Endpoint::try_from("http://[::1]:50051")
        .context("constructing endpoint")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = UnixStream::connect(&socket)
                    .await
                    .with_context(|| format!("connect to ward socket at {}", socket.display()))?;
                Ok::<_, anyhow::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .context("connect to ward daemon")?;

    Ok(WardClient::new(channel))
}
