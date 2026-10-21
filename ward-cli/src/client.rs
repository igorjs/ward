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
        .map_err(|source| DaemonUnreachable {
            socket: socket_path.to_string(),
            source,
        })?;

    Ok(WardClient::new(channel))
}

/// Docker's prefix for an error the daemon returned.
const DAEMON_ERROR_PREFIX: &str = "Error response from daemon: ";

/// The daemon could not be reached on its socket.
#[derive(Debug)]
pub struct DaemonUnreachable {
    socket: String,
    source: tonic::transport::Error,
}

impl std::fmt::Display for DaemonUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cannot connect to the Ward daemon at {}. Is wardd running?",
            self.socket
        )
    }
}

impl std::error::Error for DaemonUnreachable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Whether the daemon itself chose this status, as opposed to the transport
/// or the call machinery producing it.
fn is_server_status(status: &tonic::Status) -> bool {
    use tonic::Code;
    match status.code() {
        Code::NotFound
        | Code::AlreadyExists
        | Code::InvalidArgument
        | Code::PermissionDenied
        | Code::FailedPrecondition
        | Code::ResourceExhausted
        | Code::Unimplemented => true,
        Code::Internal => std::error::Error::source(status).is_none(),
        _ => false,
    }
}

/// The line to print for a failure in Docker's wording and without the
/// transport framing; `None` when the error should print as it is.
pub fn daemon_error_line(error: &anyhow::Error) -> Option<String> {
    if let Some(unreachable) = error.downcast_ref::<DaemonUnreachable>() {
        return Some(unreachable.to_string());
    }
    error
        .downcast_ref::<tonic::Status>()
        .filter(|status| is_server_status(status))
        .map(|status| format!("{DAEMON_ERROR_PREFIX}{}", status.message()))
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

    /// Pair a sandbox with the token its create call just returned.
    pub fn with_token(info: &SandboxInfo, token: String) -> Self {
        Self {
            token: Some(token),
            id: info.id.clone(),
            status: SandboxStatus::try_from(info.status).unwrap_or(SandboxStatus::Unspecified),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_daemon_status_when_formatted_then_docker_line_without_transport_framing() {
        // Arrange
        let error = anyhow::Error::from(tonic::Status::not_found("No such container: web"))
            .context("resolving web");

        // Act
        let line = daemon_error_line(&error);

        // Assert
        assert_eq!(
            line.as_deref(),
            Some("Error response from daemon: No such container: web")
        );
    }

    #[rstest::rstest]
    #[case::unavailable(tonic::Status::unavailable("gone"))]
    #[case::cancelled(tonic::Status::cancelled("stop"))]
    #[case::unknown(tonic::Status::unknown("odd"))]
    #[case::deadline(tonic::Status::deadline_exceeded("slow"))]
    #[case::transport_internal(tonic::Status::from_error("boom".into()))]
    fn given_transport_status_when_formatted_then_no_daemon_prefix(#[case] status: tonic::Status) {
        // Arrange
        let error = anyhow::Error::from(status);

        // Act
        let line = daemon_error_line(&error);

        // Assert
        assert_eq!(line, None);
    }

    #[rstest::rstest]
    #[case::already_exists(tonic::Status::already_exists("in use"))]
    #[case::invalid(tonic::Status::invalid_argument("bad"))]
    #[case::denied(tonic::Status::permission_denied("no"))]
    #[case::precondition(tonic::Status::failed_precondition("state"))]
    #[case::exhausted(tonic::Status::resource_exhausted("full"))]
    #[case::unimplemented(tonic::Status::unimplemented("later"))]
    #[case::internal(tonic::Status::internal("bug"))]
    fn given_server_status_when_formatted_then_daemon_prefix(#[case] status: tonic::Status) {
        // Arrange
        let message = status.message().to_string();
        let error = anyhow::Error::from(status);

        // Act
        let line = daemon_error_line(&error);

        // Assert
        assert_eq!(line, Some(format!("{DAEMON_ERROR_PREFIX}{message}")));
    }

    #[tokio::test]
    async fn given_missing_socket_when_connect_then_cannot_connect_line() {
        // Arrange
        let socket = "/nonexistent/ward.sock";

        // Act
        let error = connect(socket).await.expect_err("no daemon");

        // Assert
        assert_eq!(
            daemon_error_line(&error).as_deref(),
            Some("Cannot connect to the Ward daemon at /nonexistent/ward.sock. Is wardd running?")
        );
    }

    #[test]
    fn given_local_failure_when_formatted_then_no_daemon_line() {
        // Arrange
        let error = anyhow::anyhow!("cannot read the token store");

        // Act
        let line = daemon_error_line(&error);

        // Assert
        assert_eq!(line, None);
    }
}
