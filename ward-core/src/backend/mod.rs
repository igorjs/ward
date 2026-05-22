// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub mod image;
pub mod krunvm;

#[cfg(feature = "krunvm")]
pub(crate) mod krun_ffi;

/// Errors surfaced by any backend implementation.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("sandbox not found: {0}")]
    NotFound(String),
    #[error("image error: {0}")]
    Image(String),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

/// Convenience result alias for backend operations.
pub type Result<T> = std::result::Result<T, BackendError>;

/// Handle to a running process inside a sandbox.
#[derive(Debug)]
pub struct ProcessHandle {
    /// Opaque process identifier (string form of PID or UUID).
    pub pid: String,
    pub sandbox_id: String,
    /// Sender side for writing to the process's stdin.
    pub stdin_tx: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
    /// Receiver side for reading combined stdout/stderr events.
    pub output_rx: Option<tokio::sync::mpsc::Receiver<crate::protocol::StreamEvent>>,
}

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// VMM-agnostic interface for sandbox lifecycle and per-process operations.
///
/// `SandboxManager` is generic over this trait so we can swap libkrun for
/// Firecracker (Linux-only, snapshot-supporting) or Apple's
/// `Virtualization.framework` (macOS-only) without touching the manager,
/// gRPC, or test layers. The current implementation is `KrunvmBackend`;
/// future implementations slot in by writing the trait impl plus a small
/// constructor.
///
/// All methods are async because real backends may need to talk to a
/// hypervisor, a vsock guest agent, or a remote API. The stub
/// implementation's await points are no-ops; that's fine.
///
/// `#[async_trait]` boxes each method's future so the trait stays
/// object-safe and `SandboxManager` can hold `Arc<dyn Backend>` without
/// generic infection through every test harness. The per-call allocation
/// is far smaller than the gRPC roundtrip on every call site.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    // -- Sandbox lifecycle ----------------------------------------------

    async fn create_sandbox(
        &self,
        id: String,
        opts: &crate::protocol::CreateOpts,
    ) -> Result<crate::protocol::SandboxInfo>;

    async fn get_sandbox(&self, id: &str) -> Result<crate::protocol::SandboxInfo>;

    async fn list_sandboxes(&self) -> Result<Vec<crate::protocol::SandboxInfo>>;

    async fn remove_sandbox(&self, id: &str) -> Result<()>;

    async fn count(&self) -> Result<usize>;

    // -- Processes ------------------------------------------------------

    async fn exec(
        &self,
        sandbox_id: &str,
        command: Vec<String>,
        working_dir: Option<String>,
        env: std::collections::HashMap<String, String>,
    ) -> Result<ProcessHandle>;

    async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()>;

    // -- Snapshots ------------------------------------------------------

    async fn create_snapshot(
        &self,
        sandbox_id: &str,
        label: &str,
    ) -> Result<crate::protocol::SnapshotInfo>;

    async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()>;

    async fn list_snapshots(&self, sandbox_id: &str) -> Result<Vec<crate::protocol::SnapshotInfo>>;
}
