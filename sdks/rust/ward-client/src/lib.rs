// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! ward-client: a Rust library wrapper over the ward daemon's gRPC API.
//!
//! Idiomatic surface over a generated tonic client. The generated code
//! lives in [`pb`] and is produced from `proto/ward.proto` at build
//! time by this crate's own `build.rs` — no `path` dependency on
//! ward-core, so the Apache-2.0 / AGPL-3.0 boundary documented in
//! `Cargo.toml` stays clean.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> Result<(), ward_client::WardError> {
//! use ward_client::{CreateOptions, WardClient};
//!
//! let mut client = WardClient::connect_default().await?;
//! let sandbox = client
//!     .create_sandbox(CreateOptions {
//!         image: "alpine".into(),
//!         memory_mb: 512,
//!         cpus: 1,
//!         ..Default::default()
//!     })
//!     .await?;
//!
//! let result = client.run(&sandbox.id, &["echo", "hello"]).await?;
//! println!("stdout: {}", result.stdout);
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

/// Generated protobuf types and the tonic-built gRPC client stub.
///
/// Compiled from `proto/ward.proto` at build time. Stable surface as
/// long as protobuf field numbers and message names stay backwards
/// compatible — see ADR-004 on versioning.
pub mod pb {
    tonic::include_proto!("ward.v1");
}

use pb::ward_client::WardClient as PbClient;

// ---------------------------------------------------------------------------
// Public option / result types — idiomatic Rust over the generated `pb`.
// Callers don't have to construct protobuf messages directly.
// ---------------------------------------------------------------------------

/// Egress policy for a sandbox. Maps to [`pb::EgressMode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EgressMode {
    #[default]
    Deny,
    Open,
    Allowlist,
}

impl From<EgressMode> for i32 {
    fn from(m: EgressMode) -> Self {
        match m {
            EgressMode::Deny => pb::EgressMode::Deny as i32,
            EgressMode::Open => pb::EgressMode::Open as i32,
            EgressMode::Allowlist => pb::EgressMode::Allowlist as i32,
        }
    }
}

/// Cross-sandbox communication policy. Maps to [`pb::CommunicationMode`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommunicationMode {
    #[default]
    Deny,
    Group,
}

impl From<CommunicationMode> for i32 {
    fn from(m: CommunicationMode) -> Self {
        match m {
            CommunicationMode::Deny => pb::CommunicationMode::Deny as i32,
            CommunicationMode::Group => pb::CommunicationMode::Group as i32,
        }
    }
}

/// Options passed to [`WardClient::create_sandbox`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub image: String,
    pub cpus: u32,
    pub memory_mb: u32,
    pub timeout_seconds: u64,
    pub env: HashMap<String, String>,
    pub egress: EgressMode,
    pub egress_allowlist: Vec<String>,
    pub comms: CommunicationMode,
    pub comms_group: String,
    pub from_snapshot: Option<String>,
}

/// One ward sandbox. Returned by [`WardClient::create_sandbox`] and
/// [`WardClient::get_sandbox`].
#[derive(Debug, Clone)]
pub struct Sandbox {
    pub id: String,
    pub image: String,
    pub status: String,
}

impl From<pb::SandboxInfo> for Sandbox {
    fn from(info: pb::SandboxInfo) -> Self {
        // SandboxStatus is wire-typed as an i32 enum; surface it as a
        // short human-readable string so SDK callers don't need to
        // know the enum discriminants.
        let status = match pb::SandboxStatus::try_from(info.status).unwrap_or_default() {
            pb::SandboxStatus::Unspecified => "unspecified",
            pb::SandboxStatus::Creating => "creating",
            pb::SandboxStatus::Running => "running",
            pb::SandboxStatus::Stopped => "stopped",
            pb::SandboxStatus::Failed => "failed",
        };
        Self {
            id: info.id,
            image: info.image,
            status: status.to_string(),
        }
    }
}

/// Result of a fire-and-forget [`WardClient::run`] call.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub pid: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
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

impl From<tonic::Status> for WardError {
    fn from(s: tonic::Status) -> Self {
        // Tonic status codes map back to the SDK's narrower error shape.
        // INVALID_ARGUMENT and NOT_FOUND are common enough to surface as
        // dedicated variants; everything else collapses into `Daemon`.
        match s.code() {
            tonic::Code::NotFound => WardError::NotFound(s.message().into()),
            tonic::Code::InvalidArgument => WardError::InvalidRequest(s.message().into()),
            _ => WardError::Daemon(format!("{}: {}", s.code(), s.message())),
        }
    }
}

impl From<tonic::transport::Error> for WardError {
    fn from(e: tonic::transport::Error) -> Self {
        WardError::Transport(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// gRPC client for the ward daemon.
///
/// Cheap to clone — the underlying `tonic` channel is reference-counted.
#[derive(Clone)]
pub struct WardClient {
    inner: PbClient<Channel>,
    endpoint_label: String,
}

impl WardClient {
    /// Connect over a Unix domain socket. The default ward daemon
    /// listens on a per-user socket — see [`default_socket_path`].
    pub async fn connect_socket<P: AsRef<Path>>(path: P) -> Result<Self, WardError> {
        let socket = path.as_ref().to_path_buf();
        let label = format!("unix://{}", socket.display());

        // tonic requires *some* URI even for Unix transports — the
        // connector below is what does the real work.
        let channel = Endpoint::try_from("http://[::1]:50051")
            .map_err(|e| WardError::Transport(format!("endpoint: {e}")))?
            .connect_with_connector(service_fn(move |_: Uri| {
                let socket = socket.clone();
                async move {
                    let stream = UnixStream::connect(&socket).await.map_err(|e| {
                        std::io::Error::new(
                            e.kind(),
                            format!("connect to ward socket at {}: {e}", socket.display()),
                        )
                    })?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await?;

        Ok(Self {
            inner: PbClient::new(channel),
            endpoint_label: label,
        })
    }

    /// Connect over the user's default socket path. Honours
    /// `WARD_SOCKET` if set, otherwise mirrors the daemon's
    /// platform-default socket layout.
    pub async fn connect_default() -> Result<Self, WardError> {
        let path = default_socket_path()?;
        Self::connect_socket(path).await
    }

    /// Connect over TCP. Requires daemon-side mTLS / token auth
    /// (ADR-013) which is still in flight — this constructor exists so
    /// the call site shape is stable for downstream callers.
    pub async fn connect_tcp(target: impl Into<String>) -> Result<Self, WardError> {
        let target = target.into();
        let label = format!("tcp://{target}");
        let endpoint = Endpoint::try_from(format!("http://{target}"))
            .map_err(|e| WardError::Transport(format!("endpoint: {e}")))?;
        let channel = endpoint.connect().await?;
        Ok(Self {
            inner: PbClient::new(channel),
            endpoint_label: label,
        })
    }

    /// Inspect the configured connection target. Useful for tests +
    /// for callers that want to confirm where they're talking.
    pub fn endpoint(&self) -> &str {
        &self.endpoint_label
    }

    // ── Sandbox lifecycle ───────────────────────────────────────────

    pub async fn create_sandbox(
        &mut self,
        opts: CreateOptions,
    ) -> Result<Sandbox, WardError> {
        let req = pb::CreateSandboxRequest {
            image: opts.image,
            resources: Some(pb::ResourceLimits {
                cpus: opts.cpus,
                memory_mb: opts.memory_mb,
                pids_max: 0,
                timeout_seconds: opts.timeout_seconds,
            }),
            env: opts.env,
            comms: Some(pb::CommunicationPolicy {
                mode: opts.comms.into(),
                group: opts.comms_group,
            }),
            egress: Some(pb::EgressPolicy {
                mode: opts.egress.into(),
                domains: opts.egress_allowlist,
            }),
            mounts: Vec::new(),
            volume_ids: Vec::new(),
            from_snapshot: opts.from_snapshot.unwrap_or_default(),
        };
        let info = self.inner.create_sandbox(req).await?.into_inner();
        Ok(info.into())
    }

    pub async fn get_sandbox(&mut self, id: &str) -> Result<Sandbox, WardError> {
        let req = pb::GetSandboxRequest { id: id.into() };
        let info = self.inner.get_sandbox(req).await?.into_inner();
        Ok(info.into())
    }

    pub async fn list_sandboxes(&mut self) -> Result<Vec<Sandbox>, WardError> {
        let resp = self.inner.list_sandboxes(()).await?.into_inner();
        Ok(resp.sandboxes.into_iter().map(Sandbox::from).collect())
    }

    pub async fn remove_sandbox(&mut self, id: &str) -> Result<(), WardError> {
        let req = pb::RemoveSandboxRequest { id: id.into() };
        self.inner.remove_sandbox(req).await?;
        Ok(())
    }

    // ── Process operations ──────────────────────────────────────────

    /// Start a process in a sandbox without waiting for it to finish.
    /// Returns the synthetic pid the daemon assigned.
    pub async fn exec(
        &mut self,
        sandbox_id: &str,
        argv: &[impl AsRef<str>],
        workdir: Option<&str>,
    ) -> Result<String, WardError> {
        let req = pb::ExecRequest {
            sandbox_id: sandbox_id.into(),
            command: argv.iter().map(|s| s.as_ref().to_string()).collect(),
            working_dir: workdir.unwrap_or("").into(),
            env: HashMap::new(),
        };
        let info = self.inner.exec(req).await?.into_inner();
        Ok(info.pid)
    }

    /// Convenience wrapper that runs `argv`, streams output to
    /// completion, and returns a captured [`ExecResult`].
    pub async fn run(
        &mut self,
        sandbox_id: &str,
        argv: &[impl AsRef<str>],
    ) -> Result<ExecResult, WardError> {
        let pid = self.exec(sandbox_id, argv, None).await?;
        let mut rx = self.stream_output(sandbox_id, &pid).await?;

        let mut result = ExecResult {
            pid: pid.clone(),
            ..Default::default()
        };
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::Stdout(s) => result.stdout.push_str(&s),
                StreamEvent::Stderr(s) => result.stderr.push_str(&s),
                StreamEvent::Exit { code, duration_ms } => {
                    result.exit_code = Some(code);
                    result.duration_ms = duration_ms;
                }
            }
        }
        Ok(result)
    }

    /// Stream stdout / stderr / exit events from a running process.
    /// The returned receiver ends after the final Exit event.
    pub async fn stream_output(
        &mut self,
        sandbox_id: &str,
        pid: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>, WardError> {
        let req = pb::StreamOutputRequest {
            sandbox_id: sandbox_id.into(),
            pid: pid.into(),
        };
        let mut tonic_stream = self.inner.stream_output(req).await?.into_inner();

        // Bound the channel so a slow consumer applies backpressure
        // back to the gRPC stream rather than buffering unboundedly.
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(item) = tonic_stream.next().await {
                let Ok(ev) = item else {
                    return; // upstream error — tear the consumer channel down
                };
                let typ = pb::StreamEventType::try_from(ev.r#type).unwrap_or_default();
                let mapped = match typ {
                    pb::StreamEventType::Stdout => StreamEvent::Stdout(ev.line),
                    pb::StreamEventType::Stderr => StreamEvent::Stderr(ev.line),
                    pb::StreamEventType::Exit => StreamEvent::Exit {
                        code: ev.exit_code,
                        duration_ms: ev.duration_ms,
                    },
                    pb::StreamEventType::Unspecified => continue,
                };
                if tx.send(mapped).await.is_err() {
                    return; // consumer dropped
                }
            }
        });

        Ok(rx)
    }

    /// Send bytes to a process's stdin.
    pub async fn write_stdin(
        &mut self,
        sandbox_id: &str,
        pid: &str,
        data: impl Into<Vec<u8>>,
    ) -> Result<(), WardError> {
        let req = pb::WriteStdinRequest {
            sandbox_id: sandbox_id.into(),
            pid: pid.into(),
            data: data.into(),
        };
        self.inner.write_stdin(req).await?;
        Ok(())
    }

    /// Send a signal to a running process.
    pub async fn kill_process(&mut self, sandbox_id: &str, pid: &str) -> Result<(), WardError> {
        let req = pb::KillProcessRequest {
            sandbox_id: sandbox_id.into(),
            pid: pid.into(),
        };
        self.inner.kill_process(req).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the platform-default ward socket path.
///
/// Precedence:
///   1. `WARD_SOCKET` env var.
///   2. macOS: `$HOME/.ward/ward.sock`.
///   3. Linux: `$XDG_RUNTIME_DIR/ward/ward.sock`, falling back to
///      `/tmp/ward-$USER/ward.sock`.
pub fn default_socket_path() -> Result<PathBuf, WardError> {
    if let Ok(s) = std::env::var("WARD_SOCKET") {
        return Ok(PathBuf::from(s));
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|_| {
            WardError::InvalidRequest("HOME is not set; cannot resolve default ward socket".into())
        })?;
        Ok(PathBuf::from(home).join(".ward").join("ward.sock"))
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            return Ok(PathBuf::from(xdg).join("ward").join("ward.sock"));
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "ward".into());
        Ok(PathBuf::from("/tmp").join(format!("ward-{user}")).join("ward.sock"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(PathBuf::from("/tmp/ward.sock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn given_egress_mode_default_when_compared_then_is_deny() {
        assert_eq!(EgressMode::default(), EgressMode::Deny);
    }

    #[test]
    fn given_egress_mode_when_converted_then_matches_pb_discriminant() {
        // Regression guard: SDK enum discriminants must match the
        // protobuf enum exactly, otherwise the daemon sees the wrong
        // policy. Catch a desync at unit-test time.
        assert_eq!(i32::from(EgressMode::Deny), pb::EgressMode::Deny as i32);
        assert_eq!(i32::from(EgressMode::Open), pb::EgressMode::Open as i32);
        assert_eq!(
            i32::from(EgressMode::Allowlist),
            pb::EgressMode::Allowlist as i32
        );
    }

    #[test]
    fn given_comms_mode_when_converted_then_matches_pb_discriminant() {
        assert_eq!(
            i32::from(CommunicationMode::Deny),
            pb::CommunicationMode::Deny as i32
        );
        assert_eq!(
            i32::from(CommunicationMode::Group),
            pb::CommunicationMode::Group as i32
        );
    }

    #[test]
    fn given_sandbox_info_when_converted_then_fields_carry_over() {
        let info = pb::SandboxInfo {
            id: "abc".into(),
            image: "alpine".into(),
            status: pb::SandboxStatus::Running as i32,
            ..Default::default()
        };
        let sb: Sandbox = info.into();
        assert_eq!(sb.id, "abc");
        assert_eq!(sb.image, "alpine");
        assert_eq!(sb.status, "running");
    }

    #[test]
    fn given_sandbox_info_with_unknown_status_when_converted_then_unspecified() {
        // SandboxStatus is i32-wired; an enum value the daemon adds in a
        // future version that this SDK doesn't know about must fall back
        // gracefully rather than panic.
        let info = pb::SandboxInfo {
            id: "abc".into(),
            image: "alpine".into(),
            status: 9999,
            ..Default::default()
        };
        let sb: Sandbox = info.into();
        assert_eq!(sb.status, "unspecified");
    }

    #[test]
    fn given_invalid_argument_status_when_converted_then_becomes_invalid_request() {
        let s = tonic::Status::invalid_argument("bad image");
        match WardError::from(s) {
            WardError::InvalidRequest(m) => assert_eq!(m, "bad image"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn given_not_found_status_when_converted_then_becomes_not_found() {
        let s = tonic::Status::not_found("no sandbox");
        match WardError::from(s) {
            WardError::NotFound(m) => assert_eq!(m, "no sandbox"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
