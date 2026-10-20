// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward gRPC client connection helper.
//!
//! `tonic` does not natively support Unix sockets; you have to construct an
//! `Endpoint` with a custom connector that hands tonic a `UnixStream`. The
//! URI is a dummy ("http://[::1]") that tonic requires but never actually
//! uses — the connector intercepts the connection and routes it to the
//! socket path we capture in the closure.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{GetSandboxRequest, SandboxInfo, SandboxStatus};

use crate::tokens;

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

/// A sandbox reference resolved to its id, with the token stored under it.
pub struct Resolved {
    pub id: String,
    pub status: SandboxStatus,
    token: Option<String>,
}

impl Resolved {
    /// Pair a sandbox the daemon already described with its stored token.
    pub fn from_info(token_store: &Path, info: &SandboxInfo) -> Result<Self> {
        Ok(Self {
            token: tokens::load(token_store, &info.id)?,
            id: info.id.clone(),
            status: SandboxStatus::try_from(info.status).unwrap_or(SandboxStatus::Unspecified),
        })
    }

    /// True when the CLI token store holds a token for this sandbox.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Wrap `message` in a request carrying this sandbox's token, if any.
    pub fn request<T>(&self, message: T) -> Result<tonic::Request<T>> {
        let mut request = tonic::Request::new(message);
        tokens::attach_token(&mut request, self.token.clone())?;
        Ok(request)
    }
}

/// Resolve a name, id or id prefix to a sandbox id through `GetSandbox`.
///
/// The lookup goes out without a token: the store is keyed by id, so the
/// token can only be loaded after the daemon has said which id the
/// reference means.
pub async fn resolve(
    client: &mut WardClient<Channel>,
    token_store: &Path,
    reference: &str,
) -> Result<Resolved> {
    let info = client
        .get_sandbox(GetSandboxRequest {
            id: reference.to_string(),
        })
        .await?
        .into_inner();
    Resolved::from_info(token_store, &info)
}
