// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub mod agent;
pub mod image;
pub mod krunvm;

#[cfg(feature = "krunvm")]
pub(crate) mod krun_ffi;
#[cfg(feature = "krunvm")]
pub(crate) mod vmshim;

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
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    ResourceExhausted(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Tar chunks buffered between the two ends of a file copy.
pub const COPY_CHANNEL_CAPACITY: usize = 4;

/// Convenience result alias for backend operations.
pub type Result<T> = std::result::Result<T, BackendError>;

/// How an exec should run. The default is a plain pipe child that reports
/// line events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecMode {
    /// Run the child on a pty.
    pub tty: bool,
    /// Report unframed `Data` events instead of lines (implied by `tty`).
    pub raw: bool,
}

/// A request to a running process, sent next to stdin and kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessControl {
    /// Set the pty window size.
    Resize { rows: u32, cols: u32 },
    /// Deliver a signal number to the process.
    Signal(i32),
}

/// The processes of a sandbox: one row per process, one field per title.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTable {
    pub titles: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Guest-wide resource counters read at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuestStats {
    /// CPU time all guest processes have used so far, in nanoseconds.
    pub cpu_nanos: u64,
    pub online_cpus: u32,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    pub pids: u32,
}

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
    /// Sender for resize and signal requests, when the backend supports them.
    pub control_tx: Option<tokio::sync::mpsc::Sender<ProcessControl>>,
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

    /// The runtime config of `image`, pulling it first when needed. Backends
    /// that do not read images report an empty config.
    async fn image_config(&self, _image: &str) -> Result<image::ImageConfig> {
        Ok(image::ImageConfig::default())
    }

    /// Images in the local cache. Backends without a cache report none.
    async fn list_images(&self) -> Result<Vec<image::CachedImage>> {
        Ok(Vec::new())
    }

    /// Pull `reference` into the local image cache, reporting progress.
    async fn pull_image(
        &self,
        _reference: &str,
        _progress: Option<tokio::sync::mpsc::UnboundedSender<image::PullProgress>>,
    ) -> Result<image::CachedImage> {
        Err(BackendError::InvalidArgument(
            "this backend has no image cache".to_string(),
        ))
    }

    async fn get_sandbox(&self, id: &str) -> Result<crate::protocol::SandboxInfo>;

    async fn list_sandboxes(&self) -> Result<Vec<crate::protocol::SandboxInfo>>;

    async fn remove_sandbox(&self, id: &str) -> Result<()>;

    /// Take over a sandbox reloaded from disk after a daemon restart. It
    /// enters the backend's table with its stored options and rootfs so a
    /// later boot can find them. No VM is started.
    async fn adopt_sandbox(
        &self,
        info: crate::protocol::SandboxInfo,
        opts: crate::protocol::CreateOpts,
        rootfs: std::path::PathBuf,
    ) -> Result<()>;

    /// End the sandbox's VM and keep its rootfs and record. The status
    /// becomes `Stopped`. Stopping a sandbox with no VM up is a no-op.
    async fn stop_sandbox(&self, id: &str) -> Result<()>;

    /// Boot the sandbox's rootfs again. Any VM still up for it is ended
    /// first, so exactly one VM host process exists. The status becomes
    /// `Running`.
    async fn start_sandbox(&self, id: &str) -> Result<()>;

    async fn count(&self) -> Result<usize>;

    // -- Processes ------------------------------------------------------

    async fn exec(
        &self,
        sandbox_id: &str,
        command: Vec<String>,
        working_dir: Option<String>,
        env: std::collections::HashMap<String, String>,
        mode: ExecMode,
    ) -> Result<ProcessHandle>;

    async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()>;

    // -- File copy ------------------------------------------------------

    /// Unpack the tar stream `chunks` into the guest directory `path`.
    async fn copy_to(
        &self,
        sandbox_id: &str,
        path: &str,
        chunks: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    ) -> Result<()>;

    /// Stream a tar of the guest file or directory `path`. The guest is
    /// untrusted: callers check the tar before they use it. A failure
    /// midway arrives as an `Err` item.
    async fn copy_from(
        &self,
        sandbox_id: &str,
        path: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>>;

    // -- Introspection --------------------------------------------------

    /// List the processes running inside the guest, as `ps` with `ps_args`
    /// would. The guest refuses options it does not support.
    async fn top(&self, sandbox_id: &str, ps_args: &[String]) -> Result<ProcessTable>;

    /// Read the guest's CPU, memory and process counters.
    async fn stats(&self, sandbox_id: &str) -> Result<GuestStats>;

    // -- Snapshots ------------------------------------------------------

    async fn create_snapshot(
        &self,
        sandbox_id: &str,
        label: &str,
    ) -> Result<crate::protocol::SnapshotInfo>;

    async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()>;

    async fn list_snapshots(&self, sandbox_id: &str) -> Result<Vec<crate::protocol::SnapshotInfo>>;

    /// Return the id of the sandbox that owns `snapshot_id`. Lets callers
    /// verify a capability token against the correct sandbox before
    /// creating from a snapshot, without exposing snapshot contents.
    async fn snapshot_owner(&self, snapshot_id: &str) -> Result<String>;
}
