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
    /// Cross-sandbox pub/sub access policy. Defaults to `Deny`.
    pub comms: CommunicationPolicy,
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
// Cross-sandbox communication
// ---------------------------------------------------------------------------

/// Policy declaring how a sandbox may use the daemon's pub/sub bus.
#[derive(Debug, Clone, Default)]
pub struct CommunicationPolicy {
    pub mode: CommunicationMode,
    /// Required when `mode` is `Group`. Two sandboxes with identical group
    /// strings can publish/subscribe to each other's topics.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CommunicationMode {
    /// No publish or subscribe permitted (default-safe).
    #[default]
    Deny,
    /// Membership in a named group; co-grouped sandboxes can communicate.
    Group,
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

// ---------------------------------------------------------------------------
// Tests
//
// These tests lock in the *security-critical defaults*. Any change that
// silently weakens the isolation posture (e.g. flipping a default to "Open")
// fails the test loudly rather than slipping through review unnoticed.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // ----- Security invariants --------------------------------------------

    #[test]
    fn egress_mode_defaults_to_deny() {
        // Critical: the safe default for outbound network access is DENY.
        // Flipping this to Open or Allowlist silently breaks every sandbox
        // that did not specify an explicit policy.
        assert_eq!(EgressMode::default(), EgressMode::Deny);
    }

    #[test]
    fn egress_policy_defaults_to_deny_with_empty_domains() {
        let p = EgressPolicy::default();
        assert_eq!(p.mode, EgressMode::Deny);
        assert!(p.domains.is_empty());
    }

    #[test]
    fn communication_mode_defaults_to_deny() {
        // Same invariant as egress: cross-sandbox communication must be
        // off by default. Sandboxes opt in via CommunicationMode::Group.
        assert_eq!(CommunicationMode::default(), CommunicationMode::Deny);
    }

    #[test]
    fn communication_policy_defaults_to_deny_with_no_group() {
        let p = CommunicationPolicy::default();
        assert_eq!(p.mode, CommunicationMode::Deny);
        assert!(p.group.is_none());
    }

    // ----- Resource limits ------------------------------------------------

    #[test]
    fn resource_limits_default_is_all_zeros() {
        // Zero is the convention for "use the configured default". The
        // resource_limits validator accepts zero; the backend substitutes
        // sensible values. If this default changes, validator tests will
        // also need to update.
        let r = ResourceLimits::default();
        assert_eq!(r.cpus, 0);
        assert_eq!(r.memory_mb, 0);
        assert_eq!(r.pids_max, 0);
        assert_eq!(r.timeout_seconds, 0);
    }

    // ----- Language runtime table -----------------------------------------

    #[test]
    fn default_runtimes_returns_all_supported_languages() {
        let runtimes = default_runtimes();
        let names: Vec<&str> = runtimes.iter().map(|r| r.name).collect();
        // The set is curated, not derived — assert each known language is
        // present so a typo when adding/removing one is caught.
        assert!(names.contains(&"python"));
        assert!(names.contains(&"node"));
        assert!(names.contains(&"deno"));
        assert!(names.contains(&"ruby"));
        assert!(names.contains(&"go"));
    }

    #[test]
    fn default_runtimes_have_consistent_fields() {
        // Each entry must populate every field — empty strings would be a
        // sentinel for "forgot to fill in" and break execution later.
        for rt in default_runtimes() {
            assert!(!rt.name.is_empty(), "runtime missing name: {rt:?}");
            assert!(!rt.image.is_empty(), "runtime {} missing image", rt.name);
            assert!(
                !rt.entrypoint.is_empty(),
                "runtime {} missing entrypoint",
                rt.name
            );
            assert!(
                !rt.file_ext.is_empty(),
                "runtime {} missing file_ext",
                rt.name
            );
        }
    }

    #[test]
    fn default_runtimes_have_unique_names() {
        // Two entries with the same `name` would make lookup ambiguous.
        let runtimes = default_runtimes();
        let mut names: Vec<&str> = runtimes.iter().map(|r| r.name).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate runtime names: {names:?}"
        );
    }

    // ----- ApiError display rendering -------------------------------------
    //
    // Error messages are surfaced to clients (for the variants that don't
    // get sanitised). Their format is part of the API contract.

    #[test]
    fn api_error_display_includes_identifier() {
        let err = ApiError::SandboxNotFound("ward_abc".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("ward_abc"), "expected ID in: {msg}");
    }

    #[test]
    fn api_error_invalid_request_includes_reason() {
        let err = ApiError::InvalidRequest("topic must not be empty".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("topic must not be empty"), "got: {msg}");
    }
}
