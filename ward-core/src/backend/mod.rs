// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub mod image;
pub mod krunvm;

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
