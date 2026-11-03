// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::collections::{BTreeMap, HashMap};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Sandbox creation
// ---------------------------------------------------------------------------

/// Options for creating a new sandbox.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub spec: SandboxSpec,
}

/// User-facing identity and main-process settings kept on the sandbox record.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxSpec {
    pub name: String,
    /// Main process argv. Empty means the sandbox has no main process.
    pub command: Vec<String>,
    /// Length of the `command` prefix that came from an image Entrypoint.
    #[serde(default)]
    pub entrypoint_len: usize,
    pub workdir: String,
    /// The user the main process runs as, and the default for execs.
    #[serde(default)]
    pub user: String,
    pub tty: bool,
    /// Remove the sandbox when its main process ends.
    #[serde(default)]
    pub auto_remove: bool,
    /// Host TCP ports forwarded to the guest. The host port is the one in
    /// use, so a restart opens the same port again.
    #[serde(default)]
    pub published_ports: Vec<PublishedPort>,
    /// Manifest digest the image resolved to at creation. Empty when unknown.
    #[serde(default)]
    pub image_digest: String,
    /// User metadata from `--label`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Guest host name from `--hostname`. Empty means the short sandbox id.
    #[serde(default)]
    pub hostname: String,
    /// Signal that stops the main process, as given (`SIGINT`, `2`). Empty
    /// means SIGTERM.
    #[serde(default)]
    pub stop_signal: String,
    /// Seconds the main process gets after the stop signal. `None` uses the
    /// daemon default.
    #[serde(default)]
    pub stop_timeout: Option<i32>,
    /// What to do when the main process ends.
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Guest setup options such as `--read-only` and `--tmpfs`.
    #[serde(default)]
    pub guest: GuestOptions,
    /// Host directories shared into the guest. Filled from the sandbox's
    /// mounts when the spec is reported; never stored here.
    #[serde(skip)]
    pub mounts: Vec<Mount>,
}

/// Guest setup options of `docker run`, kept on the sandbox record.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuestOptions {
    /// Mount the root filesystem read-only.
    #[serde(default)]
    pub read_only_rootfs: bool,
    #[serde(default)]
    pub tmpfs: Vec<TmpfsMount>,
    /// Size of `/dev/shm` in bytes. 0 means [`DEFAULT_SHM_SIZE_BYTES`].
    #[serde(default)]
    pub shm_size_bytes: u64,
    /// `/etc/hosts` lines as `HOST:IP`, with `host-gateway` already resolved.
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    #[serde(default)]
    pub dns_search: Vec<String>,
    #[serde(default)]
    pub dns_options: Vec<String>,
}

/// Docker's default size for `/dev/shm`: 64 MiB.
pub const DEFAULT_SHM_SIZE_BYTES: u64 = 64 * 1024 * 1024;

impl GuestOptions {
    /// The size `/dev/shm` gets.
    #[must_use]
    pub fn shm_size(&self) -> u64 {
        if self.shm_size_bytes == 0 {
            DEFAULT_SHM_SIZE_BYTES
        } else {
            self.shm_size_bytes
        }
    }
}

/// A tmpfs mounted in the guest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TmpfsMount {
    pub path: String,
    /// Mount options as given, such as `size=64m,noexec`.
    pub options: String,
}

/// When the daemon starts a sandbox again, as in `docker run --restart`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartKind {
    #[default]
    No,
    OnFailure,
    Always,
    UnlessStopped,
}

impl RestartKind {
    /// The name `--restart` and `inspect` use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
            Self::UnlessStopped => "unless-stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestartPolicy {
    pub kind: RestartKind,
    /// Restarts `on-failure` may make; 0 is no limit.
    #[serde(default)]
    pub max_retries: u32,
}

impl RestartPolicy {
    /// Build a policy from its name and retry limit, refusing a limit on a
    /// policy that has none.
    ///
    /// # Errors
    ///
    /// Fails on an unknown name, or a limit on any policy but `on-failure`.
    pub fn from_parts(name: &str, max_retries: u32) -> Result<Self, String> {
        let kind = match name {
            "" | "no" => RestartKind::No,
            "on-failure" => RestartKind::OnFailure,
            "always" => RestartKind::Always,
            "unless-stopped" => RestartKind::UnlessStopped,
            other => return Err(format!("invalid restart policy '{other}'")),
        };
        if max_retries > 0 && kind != RestartKind::OnFailure {
            return Err(format!(
                "maximum retry count cannot be used with restart policy '{}'",
                kind.as_str()
            ));
        }
        Ok(Self { kind, max_retries })
    }

    /// Parse the `--restart` value: `NAME` or `on-failure:COUNT`.
    ///
    /// # Errors
    ///
    /// Fails like [`from_parts`](Self::from_parts), and on a count that is
    /// not a number.
    pub fn parse(text: &str) -> Result<Self, String> {
        let Some((name, count)) = text.split_once(':') else {
            return Self::from_parts(text, 0);
        };
        let count = count.parse().map_err(|_| {
            format!("invalid restart policy '{text}': the retry count is not a number")
        })?;
        Self::from_parts(name, count)
    }
}

/// Characters of the sandbox id that make the default host name, as Docker
/// uses the first 12 of a container id.
const DEFAULT_HOSTNAME_LEN: usize = 12;

impl SandboxSpec {
    /// The host name the guest gets: the requested one, else the short id.
    #[must_use]
    pub fn hostname_for(&self, id: &str) -> String {
        if self.hostname.is_empty() {
            id.chars().take(DEFAULT_HOSTNAME_LEN).collect()
        } else {
            self.hostname.clone()
        }
    }
}

/// A host TCP port forwarded to a guest port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublishedPort {
    pub host_ip: std::net::IpAddr,
    /// 0 asks for any free port when the sandbox is created.
    pub host_port: u16,
    pub container_port: u16,
}

impl PublishedPort {
    #[must_use]
    pub fn host_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.host_ip, self.host_port)
    }
}

/// A filesystem bind-mount passed into a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

// ---------------------------------------------------------------------------
// Egress
// ---------------------------------------------------------------------------

/// Network egress control policy.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct EgressPolicy {
    pub mode: EgressMode,
    /// Domain allowlist when mode is `Allowlist`.
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CommunicationPolicy {
    pub mode: CommunicationMode,
    /// Required when `mode` is `Group`. Two sandboxes with identical group
    /// strings can publish/subscribe to each other's topics.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ResourceLimits {
    pub cpus: u32,
    pub memory_mb: u32,
    pub pids_max: u32,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub exited_ttl_seconds: u64,
    /// CPU limit in billionths of a CPU. 0 means the whole `cpus`.
    #[serde(default)]
    pub nano_cpus: u64,
}

/// Bytes in one MiB.
pub const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Billionths of a CPU in one CPU.
pub const NANOS_PER_CPU: u64 = 1_000_000_000;

/// Limits `update` changes. A field left `None` is kept, and 0 removes that
/// limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitsUpdate {
    pub memory_mb: Option<u32>,
    pub nano_cpus: Option<u64>,
    pub pids_max: Option<u32>,
}

impl LimitsUpdate {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The whole CPUs the VM needs to honour `nano_cpus`.
    pub fn whole_cpus(nano_cpus: u64) -> u32 {
        u32::try_from(nano_cpus.div_ceil(NANOS_PER_CPU)).unwrap_or(u32::MAX)
    }
}

impl ResourceLimits {
    /// These limits with `update` applied. A CPU limit also sets the VM's CPU
    /// count for the next boot, rounded up.
    #[must_use]
    pub fn updated(&self, update: &LimitsUpdate) -> Self {
        let mut next = self.clone();
        if let Some(memory_mb) = update.memory_mb {
            next.memory_mb = memory_mb;
        }
        if let Some(nano_cpus) = update.nano_cpus {
            next.nano_cpus = nano_cpus;
            if nano_cpus > 0 {
                next.cpus = LimitsUpdate::whole_cpus(nano_cpus);
            }
        }
        if let Some(pids_max) = update.pids_max {
            next.pids_max = pids_max;
        }
        next
    }

    /// Fill each CPU, memory and pids field left at 0 from `defaults`.
    /// A field the request set is kept.
    pub fn with_defaults(self, defaults: &crate::config::DefaultLimits) -> Self {
        let or_default =
            |requested: u32, default: u32| if requested == 0 { default } else { requested };
        Self {
            cpus: or_default(self.cpus, defaults.cpus),
            memory_mb: or_default(self.memory_mb, defaults.memory_mb),
            pids_max: or_default(self.pids_max, defaults.pids_max),
            ..self
        }
    }
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
    pub token: String,
    pub spec: SandboxSpec,
    /// `None` until the main process has exited.
    pub exit_code: Option<i32>,
    /// When the main process last started, if it ever did.
    pub started_at: Option<SystemTime>,
    /// When the main process last ended, if it did.
    pub finished_at: Option<SystemTime>,
    /// Why the sandbox last ended, if it has.
    pub end_reason: Option<EndReason>,
    /// Restarts the daemon made since the sandbox was last started by hand.
    pub restart_count: u32,
    /// True while the sandbox's processes are frozen.
    pub paused: bool,
}

/// Why a sandbox ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndReason {
    /// The main process ended on its own.
    Exited,
    /// A signal ended the main process.
    Signal,
    Oom,
    Timeout,
    EgressPolicy,
}

impl EndReason {
    /// The name `ward inspect` and `ward events` show.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signal => "signal",
            Self::Oom => "oom",
            Self::Timeout => "timeout",
            Self::EgressPolicy => "egress-policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Creating,
    Running,
    Stopped,
    Failed,
    Exited,
    Stopping,
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
    /// Raw `wait(2)` status on an `Exit` event, when the backend reports it.
    pub wait_status: Option<i32>,
    /// Processes the guest OOM killer ended during an `Exit` event's run.
    pub oom_kills: u64,
    pub timestamp: SystemTime,
    pub duration_ms: u64,
}

/// Which output stream a raw chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawStream {
    Stdout,
    Stderr,
}

/// A piece of process output, the stream it was written to and its arrival time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub stream: RawStream,
    pub bytes: bytes::Bytes,
    /// When the daemon received the bytes.
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEventKind {
    Stdout,
    Stderr,
    Exit,
    /// Unframed output bytes, exactly as the process wrote them.
    Data {
        stream: RawStream,
        bytes: Vec<u8>,
    },
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

/// Where users report a daemon bug.
pub const ISSUES_URL: &str = "https://github.com/igorjs/ward/issues";

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
    /// The registry has no such image; the text is Docker's wording.
    #[error("{0}")]
    ImageNotFound(String),
    /// The registry or the network did not answer.
    #[error("{0}")]
    Unavailable(String),
    /// A path inside a sandbox does not exist; the text is Docker's wording.
    #[error("{0}")]
    PathNotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The guest started a copy and could not finish it.
    #[error("sandbox i/o error: {0}")]
    GuestIo(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// `id` is unknown while the other sandbox is still being created.
    #[error("sandbox name \"{name}\" is already in use")]
    NameInUse { name: String, id: Option<String> },
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("ambiguous sandbox reference: {0}")]
    AmbiguousReference(String),
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),
    #[error("address already in use: {0}")]
    AddressInUse(String),
}

impl ApiError {
    /// Translate a failed I/O call into what the user can act on. `context`
    /// names the operation and stays in the logs, never in the client text.
    pub fn from_io(context: &str, e: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::StorageFull => Self::ResourceExhausted(
                "no space left on device, free disk space and retry".to_string(),
            ),
            ErrorKind::PermissionDenied => Self::PermissionDenied(
                "the daemon cannot write to its data directory, check its ownership and permissions"
                    .to_string(),
            ),
            _ => Self::Internal(format!("{context}: {e}")),
        }
    }
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

    // ----- ApiError from I/O ----------------------------------------------

    #[test]
    fn from_io_maps_common_failures_to_actionable_variants() {
        use std::io::{Error, ErrorKind};
        let cases = [
            (
                ErrorKind::StorageFull,
                "resource exhausted: no space left on device, free disk space and retry",
            ),
            (
                ErrorKind::PermissionDenied,
                "permission denied: the daemon cannot write to its data directory, \
                 check its ownership and permissions",
            ),
            (ErrorKind::Other, "internal error: save state: boom"),
        ];
        for (kind, expected) in cases {
            // Arrange
            let io = Error::new(kind, "boom");

            // Act
            let err = ApiError::from_io("save state", &io);

            // Assert
            assert_eq!(err.to_string(), expected, "{kind:?}");
        }
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

    #[rstest::rstest]
    #[case::no("no", RestartKind::No, 0)]
    #[case::always("always", RestartKind::Always, 0)]
    #[case::unless_stopped("unless-stopped", RestartKind::UnlessStopped, 0)]
    #[case::on_failure("on-failure", RestartKind::OnFailure, 0)]
    #[case::on_failure_with_cap("on-failure:5", RestartKind::OnFailure, 5)]
    fn restart_policy_parses_docker_spellings(
        #[case] text: &str,
        #[case] kind: RestartKind,
        #[case] max_retries: u32,
    ) {
        // Act
        let policy = RestartPolicy::parse(text).expect("parse");

        // Assert
        assert_eq!(policy, RestartPolicy { kind, max_retries });
    }

    #[rstest::rstest]
    #[case::unknown("sometimes", "invalid restart policy")]
    #[case::cap_not_a_number("on-failure:many", "not a number")]
    #[case::cap_on_always("always:3", "cannot be used")]
    #[case::cap_on_no("no:1", "cannot be used")]
    fn restart_policy_refuses_bad_spellings(#[case] text: &str, #[case] said: &str) {
        // Act
        let error = RestartPolicy::parse(text).expect_err("rejected");

        // Assert
        assert!(error.contains(said), "{error}");
    }
}
