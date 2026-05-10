// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::collections::HashMap;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Sandbox creation
// ---------------------------------------------------------------------------

/// Options for creating a new sandbox.
#[derive(Debug, Clone)]
pub struct CreateOpts {
    pub image: String,
    pub mounts: Vec<Mount>,
    pub volume_ids: Vec<String>,
    pub egress: EgressPolicy,
    pub resources: ResourceLimits,
    pub env: HashMap<String, String>,
    /// If set, restore from this snapshot ID instead of a fresh image boot.
    pub from_snapshot: Option<String>,
}

/// A filesystem bind-mount passed into a sandbox.
#[derive(Debug, Clone)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

// ---------------------------------------------------------------------------
// Egress
// ---------------------------------------------------------------------------

/// Network egress control policy.
#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    pub mode: EgressMode,
    /// Domain allowlist when mode is `Allowlist`.
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EgressMode {
    /// Block all outbound traffic (default-safe).
    #[default]
    Deny,
    /// Allow only the listed domains/wildcards.
    Allowlist,
    /// Allow all outbound traffic.
    Open,
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ResourceLimits {
    pub cpus: u32,
    pub memory_mb: u32,
    pub pids_max: u32,
    pub timeout_seconds: u64,
}

// ---------------------------------------------------------------------------
// Sandbox info / status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub id: String,
    pub status: SandboxStatus,
    pub image: String,
    pub created_at: SystemTime,
    pub ip_address: Option<String>,
    pub resources: ResourceLimits,
    pub expires_at: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    Creating,
    Running,
    Stopped,
    Failed,
}

// ---------------------------------------------------------------------------
// Process execution
// ---------------------------------------------------------------------------

/// Options for a raw exec inside a sandbox.
#[derive(Debug, Clone)]
pub struct ExecOpts {
    pub sandbox_id: String,
    pub command: Vec<String>,
    pub working_dir: Option<String>,
    pub env: HashMap<String, String>,
}

/// Options for a language-aware run inside a sandbox.
#[derive(Debug, Clone)]
pub struct RunOpts {
    pub sandbox_id: String,
    pub language: String,
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: String,
    pub sandbox_id: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Output streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub kind: StreamEventKind,
    pub line: String,
    pub exit_code: Option<i32>,
    pub timestamp: SystemTime,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEventKind {
    Stdout,
    Stderr,
    Exit,
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SnapshotOpts {
    pub sandbox_id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub snapshot_id: String,
    pub sandbox_id: String,
    pub label: String,
    pub created_at: SystemTime,
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct VolumeCreateOpts {
    pub name: String,
    pub size_mb: u32,
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    pub size_mb: u32,
    pub created_at: SystemTime,
    pub mount_path: String,
}

// ---------------------------------------------------------------------------
// Language runtimes
// ---------------------------------------------------------------------------

/// A supported language runtime descriptor.
#[derive(Debug, Clone)]
pub struct LanguageRuntime {
    pub name: &'static str,
    pub image: &'static str,
    pub entrypoint: &'static str,
    pub file_ext: &'static str,
}

/// Return the built-in set of language runtimes.
pub fn default_runtimes() -> Vec<LanguageRuntime> {
    vec![
        LanguageRuntime {
            name: "python",
            image: "python:3.12-slim",
            entrypoint: "python3",
            file_ext: "py",
        },
        LanguageRuntime {
            name: "node",
            image: "node:22-slim",
            entrypoint: "node",
            file_ext: "js",
        },
        LanguageRuntime {
            name: "deno",
            image: "denoland/deno:latest",
            entrypoint: "deno run",
            file_ext: "ts",
        },
        LanguageRuntime {
            name: "ruby",
            image: "ruby:3.3-slim",
            entrypoint: "ruby",
            file_ext: "rb",
        },
        LanguageRuntime {
            name: "go",
            image: "golang:1.22-alpine",
            entrypoint: "go run",
            file_ext: "go",
        },
    ]
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("sandbox not found: {0}")]
    SandboxNotFound(String),
    #[error("volume not found: {0}")]
    VolumeNotFound(String),
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(String),
    #[error("process not found: {0}")]
    ProcessNotFound(String),
    #[error("image not found: {0}")]
    ImageNotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("internal error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Daemon metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DaemonInfo {
    pub version: String,
    pub platform: String,
    pub backend: String,
    pub arch: String,
}

#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub status: String,
    pub uptime_seconds: u64,
    pub sandbox_count: u32,
    pub checked_at: SystemTime,
}
