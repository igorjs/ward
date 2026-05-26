// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! ward-client: a Rust library wrapper over the ward daemon's gRPC API.
//!
//! Designed to be the canonical embeddable client for ward in Rust
//! applications. `ward-cli` will eventually consume this crate rather
//! than ship its own gRPC client copy; for now this crate is a scaffold
//! defining the public surface, with method bodies marked
//! `unimplemented!` until the lift-from-cli refactor lands.
//!
//! Status: first-cut scaffold. See [issue #42](https://github.com/igorjs/ward/issues/42).

use std::collections::HashMap;
use std::path::PathBuf;

/// Egress policy for a sandbox. Matches the protocol enum 1:1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EgressMode {
    #[default]
    Deny,
    Open,
    Allowlist,
}

/// Options passed to [`WardClient::create_sandbox`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub image: String,
    pub egress: EgressMode,
    pub egress_allowlist: Vec<String>,
    pub cpus: u32,
    pub memory_mb: u32,
    pub timeout_seconds: u64,
    pub env: HashMap<String, String>,
    pub from_snapshot: Option<String>,
}

/// One ward sandbox. Returned by [`WardClient::create_sandbox`].
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub id: String,
    pub image: String,
    pub status: String,
}

/// Result of a fire-and-forget [`WardClient::run`] call.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub pid: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// One event from [`WardClient::stream_output`].
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Stdout(String),
    Stderr(String),
    Exit { code: i32, duration_ms: u64 },
}

/// Errors surfaced by the client.
#[derive(Debug, thiserror::Error)]
pub enum WardError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("sandbox not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("daemon returned an error: {0}")]
    Daemon(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// gRPC client for the ward daemon.
pub struct WardClient {
    socket_path: Option<PathBuf>,
    tcp_target: Option<String>,
    // Wired up to a real `WardClient<Channel>` once the lift-from-cli
    // refactor lands. Kept as a unit field so the struct shape is
    // stable for downstream callers.
    _channel: (),
}

impl WardClient {
    /// Connect to wardd via the current user's default Unix socket
    /// (mirrors `ward-core/src/config.rs::default_socket_path`).
    pub async fn connect_default() -> Result<Self, WardError> {
        let path = default_socket_path()?;
        Self::connect_socket(path).await
    }

    /// Connect over a specific Unix domain socket.
    pub async fn connect_socket(path: PathBuf) -> Result<Self, WardError> {
        // TODO: dial the socket via tonic + hyper-util on top of tokio's
        // UnixStream once the gRPC client is in scope.
        Ok(Self {
            socket_path: Some(path),
            tcp_target: None,
            _channel: (),
        })
    }

    /// Connect over TCP. Requires daemon-side mTLS / token auth
    /// (ADR-013) which is not yet implemented in wardd.
    pub async fn connect_tcp(target: String) -> Result<Self, WardError> {
        Ok(Self {
            socket_path: None,
            tcp_target: Some(target),
            _channel: (),
        })
    }

    // ── Sandbox lifecycle ───────────────────────────────────────────

    pub async fn create_sandbox(&mut self, _opts: CreateOptions) -> Result<Sandbox, WardError> {
        unimplemented!("first-cut scaffold; wire to gRPC stub when ward-cli is refactored to consume this crate")
    }

    pub async fn remove_sandbox(&mut self, _id: &str) -> Result<(), WardError> {
        unimplemented!("first-cut scaffold; wire to gRPC stub when ward-cli is refactored to consume this crate")
    }

    // ── Process operations ──────────────────────────────────────────

    pub async fn run(&mut self, _id: &str, _argv: &[&str]) -> Result<ExecResult, WardError> {
        unimplemented!("first-cut scaffold; wire to gRPC stub when ward-cli is refactored to consume this crate")
    }

    /// Stream stdout / stderr / exit events from a running process.
    /// Returns an async stream that ends after the final Exit event.
    pub async fn stream_output(
        &mut self,
        _id: &str,
        _pid: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, WardError> {
        unimplemented!("first-cut scaffold; wire to gRPC stub when ward-cli is refactored to consume this crate")
    }

    /// Inspect the configured connection target. Useful for tests +
    /// for callers that want to confirm where they're talking.
    pub fn endpoint(&self) -> String {
        if let Some(path) = &self.socket_path {
            return format!("unix://{}", path.display());
        }
        if let Some(target) = &self.tcp_target {
            return format!("tcp://{target}");
        }
        "<unconnected>".to_string()
    }
}

/// Resolve the path wardd is listening on for the current user.
/// Mirrors `ward-core/src/config.rs::default_socket_path`.
fn default_socket_path() -> Result<PathBuf, WardError> {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(xdg).join("ward").join("ward.sock"));
    }
    match std::env::var("HOME") {
        Ok(home) => Ok(PathBuf::from(home).join(".ward").join("ward.sock")),
        Err(_) => Err(WardError::InvalidRequest(
            "HOME is not set; cannot resolve default ward socket path".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn given_socket_path_when_connect_then_endpoint_is_unix_scheme() {
        let client = WardClient::connect_socket(PathBuf::from("/tmp/ward.sock"))
            .await
            .expect("connect");
        assert_eq!(client.endpoint(), "unix:///tmp/ward.sock");
    }

    #[tokio::test]
    async fn given_tcp_target_when_connect_then_endpoint_is_tcp_scheme() {
        let client = WardClient::connect_tcp("127.0.0.1:9090".into())
            .await
            .expect("connect");
        assert_eq!(client.endpoint(), "tcp://127.0.0.1:9090");
    }

    #[test]
    fn given_egress_mode_default_when_compared_then_is_deny() {
        // Regression guard: deny-by-default is a property the SDK relies on.
        assert_eq!(EgressMode::default(), EgressMode::Deny);
    }
}
