// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! SandboxManager coordinates the backend, egress proxies, and timeout tracking.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, mpsc};

use crate::backend::{Backend, BackendError};
use crate::comms::Broker;
use crate::egress::EgressProxy;
use crate::pb::{
    ExecRequest, ProcessInfo, RunRequest, SandboxInfo as PbSandboxInfo, SandboxStatus,
};
use crate::protocol::{
    ApiError, CommunicationMode, CommunicationPolicy, CreateOpts, EgressPolicy, ResourceLimits,
    StreamEvent,
};

type Result<T> = std::result::Result<T, ApiError>;

/// Translate a BackendError to the appropriate ApiError variant so that
/// the gRPC layer can map it to the correct status code. The critical
/// distinction is NotFound, which becomes Code::NotFound to the client.
/// Wrapping everything in ApiError::Backend would collapse that signal
/// into Code::Internal — wrong for "you asked about a sandbox that does
/// not exist".
fn backend_err(e: BackendError) -> ApiError {
    match e {
        BackendError::NotFound(id) => ApiError::SandboxNotFound(id),
        other => ApiError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Per-sandbox tracking entry
// ---------------------------------------------------------------------------

struct SandboxEntry {
    #[allow(dead_code)]
    egress: EgressProxy,
    timeout_handle: Option<tokio::task::JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// Per-process tracking entry
// ---------------------------------------------------------------------------

/// State held for each process spawned via exec/run. The output receiver is
/// wrapped in `Mutex<Option<...>>` so the first `stream_output` call can take
/// it; a second call sees `None` and returns InvalidRequest. The stdin
/// sender is plain `Option<Sender>` because Sender is Clone — many concurrent
/// WriteStdin calls can share it. `None` represents a process that doesn't
/// accept stdin at all (real backend may produce these).
struct ProcessRecord {
    sandbox_id: String,
    output_rx: Mutex<Option<mpsc::Receiver<StreamEvent>>>,
    stdin_tx: Option<mpsc::Sender<bytes::Bytes>>,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Coordinates sandbox lifecycle across the backend and supporting subsystems.
pub struct SandboxManager {
    backend: Arc<dyn Backend>,
    /// Pub/sub broker shared with the gRPC layer. Manager owns lifecycle
    /// notifications (register on create, deregister on remove); gRPC owns
    /// the per-RPC routing (publish/subscribe/log).
    broker: Arc<Broker>,
    entries: Arc<RwLock<HashMap<String, SandboxEntry>>>,
    /// Maximum concurrent sandboxes. Prevents resource exhaustion from unbounded creation.
    max_sandboxes: usize,
    /// Process records keyed by pid. Populated by exec/run; drained by
    /// stream_output. Lives for the lifetime of the manager — small leak
    /// bounded by sandbox lifetime, cleaned up when the sandbox is removed.
    processes: Arc<RwLock<HashMap<String, ProcessRecord>>>,
}

impl SandboxManager {
    pub fn new(backend: Arc<dyn Backend>, broker: Arc<Broker>, max_sandboxes: usize) -> Self {
        Self {
            backend,
            broker,
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_sandboxes,
            processes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Borrow the shared broker. Useful for the gRPC layer which needs
    /// to call publish/subscribe/log without going through the manager.
    pub fn broker(&self) -> Arc<Broker> {
        Arc::clone(&self.broker)
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Create a new sandbox.
    pub async fn create(&self, req: crate::pb::CreateSandboxRequest) -> Result<PbSandboxInfo> {
        crate::validate::image_ref(&req.image)?;
        if let Some(ref r) = req.resources {
            crate::validate::resource_limits(r.cpus, r.memory_mb, r.pids_max, r.timeout_seconds)?;
        }

        // Validate communication policy: if a group is specified or mode is
        // GROUP, the group name must be present and well-formed.
        if let Some(ref c) = req.comms {
            let mode = c.mode();
            if mode == crate::pb::CommunicationMode::Group {
                crate::validate::group_name(&c.group)?;
            }
        }

        // Enforce sandbox cap to prevent resource exhaustion.
        let current = self.entries.read().await.len();
        if current >= self.max_sandboxes {
            return Err(ApiError::InvalidRequest(format!(
                "sandbox limit reached ({}/{})",
                current, self.max_sandboxes,
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();

        let egress_policy = req.egress.map(pb_egress_to_protocol).unwrap_or_default();

        let resources = req
            .resources
            .map(pb_resources_to_protocol)
            .unwrap_or_default();

        let comms = req.comms.map(pb_comms_to_protocol).unwrap_or_default();

        let opts = CreateOpts {
            image: req.image.clone(),
            mounts: vec![],
            volume_ids: req.volume_ids.clone(),
            egress: egress_policy.clone(),
            resources,
            env: req.env.clone(),
            from_snapshot: if req.from_snapshot.is_empty() {
                None
            } else {
                Some(req.from_snapshot.clone())
            },
            comms: comms.clone(),
        };

        let info = self
            .backend
            .create_sandbox(id.clone(), &opts)
            .await
            .map_err(backend_err)?;

        let egress = EgressProxy::new(id.clone(), egress_policy);

        // Register a timeout watcher if requested.
        let timeout_handle = if info.resources.timeout_seconds > 0 {
            let backend = Arc::clone(&self.backend);
            let sandbox_id = id.clone();
            let secs = info.resources.timeout_seconds;
            Some(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                tracing::info!(sandbox_id, "sandbox timeout reached, removing");
                let _ = backend.remove_sandbox(&sandbox_id).await;
            }))
        } else {
            None
        };

        self.entries.write().await.insert(
            id.clone(),
            SandboxEntry {
                egress,
                timeout_handle,
            },
        );

        // Register the comms policy with the broker so publish/subscribe
        // calls from this sandbox have something to match against. Done
        // after the entries-insert so we don't leak broker state if the
        // local registration fails — though entries.insert can't actually
        // fail here, the order keeps cleanup symmetric with remove().
        self.broker.register_sandbox(id, comms).await;

        Ok(protocol_info_to_pb(info))
    }

    /// Retrieve info for an existing sandbox.
    pub async fn get(&self, id: &str) -> Result<PbSandboxInfo> {
        crate::validate::entity_id(id, "sandbox")?;
        let info = self.backend.get_sandbox(id).await.map_err(backend_err)?;
        Ok(protocol_info_to_pb(info))
    }

    /// List all sandboxes.
    pub async fn list(&self) -> Result<Vec<PbSandboxInfo>> {
        let infos = self.backend.list_sandboxes().await.map_err(backend_err)?;
        Ok(infos.into_iter().map(protocol_info_to_pb).collect())
    }

    /// Remove a sandbox.
    pub async fn remove(&self, id: &str) -> Result<()> {
        crate::validate::entity_id(id, "sandbox")?;
        // Cancel any pending timeout task.
        if let Some(entry) = self.entries.write().await.remove(id) {
            if let Some(handle) = entry.timeout_handle {
                handle.abort();
            }
        }

        // Drop any process records that belong to this sandbox so they
        // do not accumulate over a long-lived daemon's lifetime. The
        // backend resources are torn down below; the in-memory bookkeeping
        // goes here.
        self.processes
            .write()
            .await
            .retain(|_, rec| rec.sandbox_id != id);

        // Drop the comms registration too: drops the policy, the audit
        // log, and any active subscriptions owned by this sandbox.
        self.broker.deregister_sandbox(id).await;

        self.backend.remove_sandbox(id).await.map_err(backend_err)
    }

    /// Return the number of active sandboxes.
    pub async fn count(&self) -> Result<usize> {
        self.backend.count().await.map_err(backend_err)
    }

    // -----------------------------------------------------------------------
    // Snapshots
    // -----------------------------------------------------------------------

    /// Take a snapshot of an existing sandbox. The label is free-form
    /// (empty is allowed); callers use it to remember what the snapshot
    /// represents.
    pub async fn create_snapshot(
        &self,
        sandbox_id: &str,
        label: &str,
    ) -> Result<crate::protocol::SnapshotInfo> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        self.backend
            .create_snapshot(sandbox_id, label)
            .await
            .map_err(backend_err)
    }

    /// Restore a sandbox from one of its snapshots. The backend rejects
    /// cross-sandbox restore as NotFound; we translate that to
    /// SnapshotNotFound here so the gRPC layer maps it to "snapshot not
    /// found" rather than "sandbox not found" — the user's mental model
    /// is "the snapshot doesn't exist for this sandbox", which is true
    /// either way.
    pub async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        crate::validate::entity_id(snapshot_id, "snapshot")?;
        self.backend
            .restore_snapshot(sandbox_id, snapshot_id)
            .await
            .map_err(|e| match e {
                BackendError::NotFound(id) => ApiError::SnapshotNotFound(id),
                other => ApiError::Backend(other.to_string()),
            })
    }

    /// List all snapshots taken from a given sandbox.
    pub async fn list_snapshots(
        &self,
        sandbox_id: &str,
    ) -> Result<Vec<crate::protocol::SnapshotInfo>> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        self.backend
            .list_snapshots(sandbox_id)
            .await
            .map_err(backend_err)
    }

    // -----------------------------------------------------------------------
    // Process execution
    // -----------------------------------------------------------------------

    /// Execute an arbitrary command inside a sandbox.
    pub async fn exec(&self, req: ExecRequest) -> Result<ProcessInfo> {
        crate::validate::entity_id(&req.sandbox_id, "sandbox")?;
        crate::validate::exec_command(&req.command)?;
        let handle = self
            .backend
            .exec(
                &req.sandbox_id,
                req.command.clone(),
                if req.working_dir.is_empty() {
                    None
                } else {
                    Some(req.working_dir.clone())
                },
                req.env.clone(),
            )
            .await
            .map_err(backend_err)?;

        // Park both channels under the pid: StreamOutput takes the receiver,
        // WriteStdin uses the sender. Either may be None if the backend
        // produced a process without that channel attached.
        let pid = handle.pid.clone();
        let record = ProcessRecord {
            sandbox_id: req.sandbox_id.clone(),
            output_rx: Mutex::new(handle.output_rx),
            stdin_tx: handle.stdin_tx,
        };
        self.processes.write().await.insert(pid.clone(), record);

        Ok(ProcessInfo {
            pid,
            sandbox_id: req.sandbox_id,
            status: "running".to_string(),
        })
    }

    /// Take the output receiver for a previously-started process. Single-
    /// consumer: a second call returns InvalidRequest. The caller is
    /// expected to drain the channel and translate events into whatever
    /// stream type the transport needs.
    pub async fn stream_output(
        &self,
        sandbox_id: &str,
        pid: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        crate::validate::entity_id(pid, "process")?;

        let guard = self.processes.read().await;
        let record = guard
            .get(pid)
            .ok_or_else(|| ApiError::ProcessNotFound(pid.to_string()))?;

        // Defence in depth: a caller must address the process by the
        // sandbox that owns it. Hiding pids across sandboxes prevents
        // cross-tenant log harvesting if pids are guessed or leaked.
        if record.sandbox_id != sandbox_id {
            return Err(ApiError::ProcessNotFound(pid.to_string()));
        }

        record
            .output_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| ApiError::InvalidRequest("output stream already consumed".into()))
    }

    /// Signal a process to terminate and drop its bookkeeping.
    ///
    /// Two steps: the backend is asked to signal (no-op in stub mode), then
    /// the ProcessRecord is removed from the map so its channels drop. From
    /// the user's perspective the pid disappears: subsequent stream_output,
    /// write_stdin, and kill_process calls all return ProcessNotFound.
    pub async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        crate::validate::entity_id(pid, "process")?;

        // Verify ownership in a read scope first — a kill of an unknown or
        // cross-sandbox pid should fail with ProcessNotFound BEFORE the
        // backend is touched. Taking the write lock conditionally avoids
        // racing two concurrent kill calls into the backend.
        {
            let guard = self.processes.read().await;
            let record = guard
                .get(pid)
                .ok_or_else(|| ApiError::ProcessNotFound(pid.to_string()))?;
            if record.sandbox_id != sandbox_id {
                return Err(ApiError::ProcessNotFound(pid.to_string()));
            }
        }

        self.backend
            .kill_process(sandbox_id, pid)
            .await
            .map_err(backend_err)?;

        // Drop the record. stdin_tx drops here (drain task exits), output_rx
        // either was already taken or drops too (consumer sees None).
        self.processes.write().await.remove(pid);

        Ok(())
    }

    /// Forward bytes to a running process's stdin.
    ///
    /// Returns ProcessNotFound if the pid is unknown, scoped to a different
    /// sandbox, or no longer accepting input (channel closed). Empty data
    /// is a valid no-op — callers occasionally use it as a connectivity
    /// probe before streaming real input.
    pub async fn write_stdin(&self, sandbox_id: &str, pid: &str, data: bytes::Bytes) -> Result<()> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        crate::validate::entity_id(pid, "process")?;

        let guard = self.processes.read().await;
        let record = guard
            .get(pid)
            .ok_or_else(|| ApiError::ProcessNotFound(pid.to_string()))?;
        if record.sandbox_id != sandbox_id {
            return Err(ApiError::ProcessNotFound(pid.to_string()));
        }

        let tx = record
            .stdin_tx
            .as_ref()
            .ok_or_else(|| ApiError::InvalidRequest("process does not accept stdin".into()))?;

        // Send failure means the consumer side dropped — the process is
        // effectively gone from the user's perspective. Surfacing as
        // ProcessNotFound keeps callers from special-casing "closed-mid-
        // write" separately from "unknown pid".
        tx.send(data)
            .await
            .map_err(|_| ApiError::ProcessNotFound(pid.to_string()))?;

        Ok(())
    }

    /// Run a language snippet inside a sandbox.
    pub async fn run(&self, req: RunRequest) -> Result<ProcessInfo> {
        crate::validate::entity_id(&req.sandbox_id, "sandbox")?;
        crate::validate::language_name(&req.language)?;
        use crate::protocol::default_runtimes;

        let runtime = default_runtimes()
            .into_iter()
            .find(|r| r.name.eq_ignore_ascii_case(&req.language))
            .ok_or_else(|| {
                ApiError::InvalidRequest(format!("unsupported language: {}", req.language))
            })?;

        // Write code to a temp file inside the sandbox via exec.
        // TODO: implement file-write channel; for now stub the exec.
        let command = vec![
            runtime.entrypoint.to_string(),
            format!("/tmp/ward_run.{}", runtime.file_ext),
        ];

        let exec_req = ExecRequest {
            sandbox_id: req.sandbox_id.clone(),
            command,
            working_dir: "/tmp".to_string(),
            env: HashMap::new(),
        };

        self.exec(exec_req).await
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn pb_egress_to_protocol(pb: crate::pb::EgressPolicy) -> EgressPolicy {
    use crate::pb::EgressMode as PbMode;
    use crate::protocol::EgressMode;

    let mode = match pb.mode() {
        PbMode::Deny => EgressMode::Deny,
        PbMode::Allowlist => EgressMode::Allowlist,
        PbMode::Open => EgressMode::Open,
        PbMode::Unspecified => EgressMode::Deny,
    };

    EgressPolicy {
        mode,
        domains: pb.domains,
    }
}

fn pb_comms_to_protocol(pb: crate::pb::CommunicationPolicy) -> CommunicationPolicy {
    use crate::pb::CommunicationMode as PbMode;

    // Default to Deny on Unspecified – matches the egress pattern where
    // missing or unknown policy means "no access".
    let mode = match pb.mode() {
        PbMode::Group => CommunicationMode::Group,
        PbMode::Deny | PbMode::Unspecified => CommunicationMode::Deny,
    };

    let group = if pb.group.is_empty() {
        None
    } else {
        Some(pb.group)
    };

    CommunicationPolicy { mode, group }
}

fn pb_resources_to_protocol(pb: crate::pb::ResourceLimits) -> ResourceLimits {
    ResourceLimits {
        cpus: pb.cpus,
        memory_mb: pb.memory_mb,
        pids_max: pb.pids_max,
        timeout_seconds: pb.timeout_seconds,
    }
}

fn protocol_info_to_pb(info: crate::protocol::SandboxInfo) -> PbSandboxInfo {
    use crate::protocol::SandboxStatus as ProtocolStatus;

    let status = match info.status {
        ProtocolStatus::Creating => SandboxStatus::Creating,
        ProtocolStatus::Running => SandboxStatus::Running,
        ProtocolStatus::Stopped => SandboxStatus::Stopped,
        ProtocolStatus::Failed => SandboxStatus::Failed,
    } as i32;

    let created_at = Some(system_time_to_timestamp(info.created_at));
    let expires_at = info.expires_at.map(system_time_to_timestamp);

    PbSandboxInfo {
        id: info.id,
        status,
        image: info.image,
        created_at,
        ip_address: info.ip_address.unwrap_or_default(),
        resources: None,
        expires_at,
    }
}

fn system_time_to_timestamp(t: std::time::SystemTime) -> prost_types::Timestamp {
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// SandboxManager unit tests verify the in-process state machine — capacity
// cap, timeout-task cancellation, and the conversion helpers — against the
// stub backend (KrunvmBackend without the `krunvm` feature). Integration
// tests for the same behaviour over gRPC live in tests/grpc_sandbox.rs.
//
// BDD names with AAA bodies. Each test builds its own manager pointed at a
// per-test data_dir so they parallelise without sharing state.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::{
        CommunicationMode as PbCommunicationMode, CommunicationPolicy as PbCommunicationPolicy,
        CreateSandboxRequest, EgressMode as PbEgressMode, EgressPolicy as PbEgressPolicy,
        ResourceLimits as PbResourceLimits,
    };
    use crate::protocol::StreamEventKind;
    use pretty_assertions::assert_eq;

    /// Build a fresh SandboxManager pointed at a per-test data_dir.
    /// Leaks the TempDir intentionally: tokio's async fs API outlives any
    /// test-local scope, and the OS cleans /tmp on its own schedule.
    fn build_manager(max_sandboxes: usize) -> Arc<SandboxManager> {
        use crate::backend::krunvm::KrunvmBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::new(path));
        let broker = Arc::new(Broker::new());
        Arc::new(SandboxManager::new(backend, broker, max_sandboxes))
    }

    fn create_req(image: &str) -> CreateSandboxRequest {
        CreateSandboxRequest {
            image: image.to_string(),
            ..Default::default()
        }
    }

    // ----- create --------------------------------------------------------

    #[tokio::test]
    async fn given_empty_manager_when_create_sandbox_then_returns_info_with_uuid() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let info = mgr
            .create(create_req("alpine:latest"))
            .await
            .expect("create should succeed");

        // Assert: the daemon assigns a UUID and echoes the image back.
        assert_eq!(info.id.len(), 36);
        assert_eq!(info.image, "alpine:latest");
        // SandboxStatus::Creating = 1 in the generated enum.
        assert_eq!(info.status, crate::pb::SandboxStatus::Creating as i32);
    }

    #[tokio::test]
    async fn given_invalid_image_when_create_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act: empty image violates the validator's non-empty rule.
        let err = mgr
            .create(create_req(""))
            .await
            .expect_err("empty image must be rejected");

        // Assert: validation produces InvalidRequest so the gRPC layer
        // maps it to InvalidArgument.
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_path_traversal_image_when_create_then_returns_invalid_request() {
        // Arrange: regression guard for the path-traversal validator rule.
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .create(create_req("../../etc/passwd"))
            .await
            .expect_err("path traversal must be rejected");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_oversized_cpus_when_create_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act: 9999 cpus exceeds MAX_CPUS=64.
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            resources: Some(PbResourceLimits {
                cpus: 9999,
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = mgr.create(req).await.expect_err("over-cap cpus");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_group_mode_without_group_name_when_create_then_returns_invalid_request() {
        // Arrange: CommunicationMode::Group requires a non-empty group string.
        let mgr = build_manager(4);

        // Act
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            comms: Some(PbCommunicationPolicy {
                mode: PbCommunicationMode::Group as i32,
                group: String::new(),
            }),
            ..Default::default()
        };
        let err = mgr.create(req).await.expect_err("group without name");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_manager_at_capacity_when_create_then_returns_invalid_request_with_limit() {
        // Arrange: fill to capacity.
        let mgr = build_manager(2);
        mgr.create(create_req("alpine:1")).await.unwrap();
        mgr.create(create_req("alpine:2")).await.unwrap();

        // Act
        let err = mgr
            .create(create_req("alpine:3"))
            .await
            .expect_err("third over cap");

        // Assert: cap surfaces as InvalidRequest mentioning "limit" so
        // users can grep their logs.
        match err {
            ApiError::InvalidRequest(msg) => {
                assert!(msg.contains("limit"), "expected 'limit' in: {msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ----- get -----------------------------------------------------------

    #[tokio::test]
    async fn given_created_sandbox_when_get_by_id_then_returns_same_info() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let fetched = mgr.get(&created.id).await.expect("get");

        // Assert: id and image round-trip identically.
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.image, created.image);
    }

    #[tokio::test]
    async fn given_unknown_id_when_get_sandbox_then_returns_sandbox_not_found() {
        // Arrange: well-formed UUID the manager has never seen.
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .get("00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown id");

        // Assert
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_malformed_id_when_get_sandbox_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act: non-hex characters fail validate::entity_id before lookup.
        let err = mgr
            .get("not-a-valid-uuid-zzzz")
            .await
            .expect_err("malformed id");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    // ----- list ----------------------------------------------------------

    #[tokio::test]
    async fn given_empty_manager_when_list_then_returns_empty_vec() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let sandboxes = mgr.list().await.expect("list");

        // Assert
        assert!(sandboxes.is_empty());
    }

    #[tokio::test]
    async fn given_three_sandboxes_when_list_then_returns_all_three() {
        // Arrange
        let mgr = build_manager(4);
        mgr.create(create_req("alpine:a")).await.unwrap();
        mgr.create(create_req("alpine:b")).await.unwrap();
        mgr.create(create_req("alpine:c")).await.unwrap();

        // Act
        let mut sandboxes = mgr.list().await.expect("list");

        // Assert: every image appears. Sort before compare because HashMap
        // order is unspecified.
        sandboxes.sort_by(|x, y| x.image.cmp(&y.image));
        let images: Vec<&str> = sandboxes.iter().map(|s| s.image.as_str()).collect();
        assert_eq!(images, vec!["alpine:a", "alpine:b", "alpine:c"]);
    }

    // ----- remove --------------------------------------------------------

    #[tokio::test]
    async fn given_created_sandbox_when_remove_then_get_returns_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        mgr.remove(&s.id).await.expect("remove");

        // Assert
        let err = mgr.get(&s.id).await.expect_err("must be gone");
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_unknown_id_when_remove_sandbox_then_returns_sandbox_not_found() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .remove("00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown id");

        // Assert
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_sandbox_removed_when_create_then_cap_slot_is_freed() {
        // Arrange: regression for cap-counter bookkeeping. Fill the cap,
        // remove one, then create one more.
        let mgr = build_manager(2);
        let s1 = mgr.create(create_req("alpine:1")).await.unwrap();
        let _s2 = mgr.create(create_req("alpine:2")).await.unwrap();

        // Act
        mgr.remove(&s1.id).await.unwrap();
        let s3 = mgr.create(create_req("alpine:3")).await;

        // Assert
        assert!(s3.is_ok(), "removing a sandbox must free a cap slot");
    }

    // ----- conversion helpers --------------------------------------------

    #[test]
    fn given_pb_egress_unspecified_when_convert_then_protocol_is_deny() {
        // Arrange: regression guard for the security default. If the
        // Unspecified arm ever maps to anything but Deny, sandboxes that
        // omitted an egress policy would silently get more access.
        let pb = PbEgressPolicy {
            mode: PbEgressMode::Unspecified as i32,
            domains: vec![],
        };

        // Act
        let result = pb_egress_to_protocol(pb);

        // Assert
        assert_eq!(result.mode, EgressPolicy::default().mode);
        assert!(result.domains.is_empty());
    }

    #[test]
    fn given_pb_egress_open_when_convert_then_protocol_is_open() {
        // Arrange
        let pb = PbEgressPolicy {
            mode: PbEgressMode::Open as i32,
            domains: vec!["ignored.example".into()],
        };

        // Act
        let result = pb_egress_to_protocol(pb);

        // Assert: domain list is carried through even though Open ignores it.
        assert_eq!(result.mode, crate::protocol::EgressMode::Open);
        assert_eq!(result.domains, vec!["ignored.example"]);
    }

    #[test]
    fn given_pb_egress_allowlist_when_convert_then_domains_round_trip() {
        // Arrange
        let pb = PbEgressPolicy {
            mode: PbEgressMode::Allowlist as i32,
            domains: vec!["api.example.com".into(), "*.cdn.net".into()],
        };

        // Act
        let result = pb_egress_to_protocol(pb);

        // Assert
        assert_eq!(result.mode, crate::protocol::EgressMode::Allowlist);
        assert_eq!(result.domains, vec!["api.example.com", "*.cdn.net"]);
    }

    #[test]
    fn given_pb_comms_unspecified_when_convert_then_protocol_is_deny() {
        // Arrange: same security-default invariant as egress.
        let pb = PbCommunicationPolicy {
            mode: PbCommunicationMode::Unspecified as i32,
            group: String::new(),
        };

        // Act
        let result = pb_comms_to_protocol(pb);

        // Assert
        assert_eq!(result.mode, CommunicationMode::Deny);
        assert!(result.group.is_none());
    }

    #[test]
    fn given_pb_comms_group_with_name_when_convert_then_group_is_populated() {
        // Arrange
        let pb = PbCommunicationPolicy {
            mode: PbCommunicationMode::Group as i32,
            group: "build-team".into(),
        };

        // Act
        let result = pb_comms_to_protocol(pb);

        // Assert
        assert_eq!(result.mode, CommunicationMode::Group);
        assert_eq!(result.group.as_deref(), Some("build-team"));
    }

    #[test]
    fn given_pb_comms_empty_group_when_convert_then_group_is_none() {
        // Arrange: empty string group → None on the Rust side. Distinguishes
        // "no group specified" from "group named empty-string".
        let pb = PbCommunicationPolicy {
            mode: PbCommunicationMode::Deny as i32,
            group: String::new(),
        };

        // Act
        let result = pb_comms_to_protocol(pb);

        // Assert
        assert!(result.group.is_none());
    }

    #[test]
    fn given_pb_resources_when_convert_then_every_field_round_trips() {
        // Arrange
        let pb = PbResourceLimits {
            cpus: 4,
            memory_mb: 8192,
            pids_max: 256,
            timeout_seconds: 3600,
        };

        // Act
        let result = pb_resources_to_protocol(pb);

        // Assert: each numeric field is copied verbatim. No coercion, no
        // implicit defaulting — the validator already vetted the bounds.
        assert_eq!(result.cpus, 4);
        assert_eq!(result.memory_mb, 8192);
        assert_eq!(result.pids_max, 256);
        assert_eq!(result.timeout_seconds, 3600);
    }

    // ----- exec ----------------------------------------------------------

    #[tokio::test]
    async fn given_existing_sandbox_when_exec_then_returns_process_info_with_pid() {
        // Arrange: create a sandbox so exec has a target.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let resp = mgr
            .exec(crate::pb::ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["echo".into(), "hello".into()],
                working_dir: String::new(),
                env: Default::default(),
            })
            .await
            .expect("exec");

        // Assert: a UUID-shaped pid is returned, status is "running",
        // sandbox_id round-trips. The stub does not actually execute
        // the command, but the gRPC contract is identical.
        assert_eq!(resp.pid.len(), 36);
        assert_eq!(resp.sandbox_id, s.id);
        assert_eq!(resp.status, "running");
    }

    #[tokio::test]
    async fn given_empty_command_when_exec_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act: empty command must be rejected by the validator before
        // it reaches the backend (where it would otherwise spawn nothing).
        let err = mgr
            .exec(crate::pb::ExecRequest {
                sandbox_id: s.id,
                command: vec![],
                working_dir: String::new(),
                env: Default::default(),
            })
            .await
            .expect_err("empty command must be rejected");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_malformed_sandbox_id_when_exec_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .exec(crate::pb::ExecRequest {
                sandbox_id: "not-a-uuid-zzzz".into(),
                command: vec!["echo".into()],
                working_dir: String::new(),
                env: Default::default(),
            })
            .await
            .expect_err("malformed id");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_exec_then_returns_sandbox_not_found() {
        // Arrange: well-formed UUID, but no sandbox with this ID exists.
        // Exercises the backend_err mapping for BackendError::NotFound.
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .exec(crate::pb::ExecRequest {
                sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
                command: vec!["echo".into()],
                working_dir: String::new(),
                env: Default::default(),
            })
            .await
            .expect_err("unknown sandbox");

        // Assert: SandboxNotFound (not the generic Backend variant) —
        // regression guard for the manager's error-translation helper.
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    // ----- run -----------------------------------------------------------

    #[tokio::test]
    async fn given_existing_sandbox_when_run_python_then_returns_process_info() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("python:3.12-slim")).await.unwrap();

        // Act
        let resp = mgr
            .run(crate::pb::RunRequest {
                sandbox_id: s.id.clone(),
                language: "python".into(),
                code: "print('hi')".into(),
            })
            .await
            .expect("run");

        // Assert: same contract as exec — pid + status from the stub.
        assert_eq!(resp.pid.len(), 36);
        assert_eq!(resp.sandbox_id, s.id);
        assert_eq!(resp.status, "running");
    }

    #[tokio::test]
    async fn given_unsupported_language_when_run_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act: "cobol" is not in default_runtimes(); the runtime lookup
        // returns InvalidRequest before the backend is ever called.
        let err = mgr
            .run(crate::pb::RunRequest {
                sandbox_id: s.id,
                language: "cobol".into(),
                code: "DISPLAY 'hello'".into(),
            })
            .await
            .expect_err("unsupported language");

        // Assert
        match err {
            ApiError::InvalidRequest(msg) => {
                assert!(
                    msg.contains("unsupported language"),
                    "expected message to mention 'unsupported language': {msg}",
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn given_invalid_language_name_when_run_then_returns_invalid_request() {
        // Arrange: dash in the language name fails the language_name
        // validator before the runtime lookup even runs.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let err = mgr
            .run(crate::pb::RunRequest {
                sandbox_id: s.id,
                language: "py-thon".into(),
                code: "print('hi')".into(),
            })
            .await
            .expect_err("invalid language name");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_run_case_insensitive_language_when_lookup_then_matches() {
        // Arrange: regression for `eq_ignore_ascii_case` matching against
        // the runtime table. Users may type "Python" or "PYTHON" — both
        // should resolve.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("python:3.12")).await.unwrap();

        // Act
        let resp = mgr
            .run(crate::pb::RunRequest {
                sandbox_id: s.id,
                language: "PYTHON".into(),
                code: "print(1)".into(),
            })
            .await;

        // Assert
        assert!(resp.is_ok());
    }

    // ----- stream_output -------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_stream_output_then_drains_scripted_stdout_and_exit() {
        // Arrange: exec parks the receiver under the pid; we take it back
        // out and confirm the scripted stub events come through. The
        // first event is a Stdout line; the second is the Exit(0) marker.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["echo".into(), "hi".into()],
                ..Default::default()
            })
            .await
            .expect("exec");

        // Act
        let mut rx = mgr
            .stream_output(&s.id, &proc.pid)
            .await
            .expect("stream_output");

        let first = rx.recv().await.expect("first event");
        let second = rx.recv().await.expect("second event");
        let after_close = rx.recv().await;

        // Assert: shape only — the stub may evolve its line text, but
        // (Stdout, then Exit, then None) is the contract.
        assert_eq!(first.kind, StreamEventKind::Stdout);
        assert_eq!(second.kind, StreamEventKind::Exit);
        assert_eq!(second.exit_code, Some(0));
        assert!(after_close.is_none(), "channel must close after Exit");
    }

    #[tokio::test]
    async fn given_unknown_pid_when_stream_output_then_process_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act: well-formed UUID that was never produced by an exec call.
        let err = mgr
            .stream_output(&s.id, "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown pid");

        // Assert
        assert!(
            matches!(err, ApiError::ProcessNotFound(_)),
            "expected ProcessNotFound, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn given_pid_owned_by_other_sandbox_when_stream_output_then_process_not_found() {
        // Arrange: two sandboxes, one process under the first. Asking
        // for that pid from the second sandbox's perspective must hide
        // its existence — pid is scoped to its owning sandbox.
        let mgr = build_manager(4);
        let s1 = mgr.create(create_req("alpine:1")).await.unwrap();
        let s2 = mgr.create(create_req("alpine:2")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s1.id.clone(),
                command: vec!["echo".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act: ask sandbox 2 about a pid that belongs to sandbox 1.
        let err = mgr
            .stream_output(&s2.id, &proc.pid)
            .await
            .expect_err("cross-sandbox pid");

        // Assert: NotFound — leaking the existence of another sandbox's
        // pid would be a tenant-isolation regression.
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_stream_output_consumed_when_called_again_then_invalid_request() {
        // Arrange: single-consumer contract. Once a caller takes the
        // receiver, subsequent calls see None.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["echo".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        let _first = mgr
            .stream_output(&s.id, &proc.pid)
            .await
            .expect("first call");

        // Act
        let err = mgr
            .stream_output(&s.id, &proc.pid)
            .await
            .expect_err("second call");

        // Assert
        assert!(
            matches!(err, ApiError::InvalidRequest(_)),
            "expected InvalidRequest, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn given_sandbox_removed_when_stream_output_for_old_pid_then_process_not_found() {
        // Arrange: removing a sandbox must drop its process records so
        // they do not accumulate. Asking for a pid afterwards looks like
        // it never existed.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["echo".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        mgr.remove(&s.id).await.expect("remove");
        let err = mgr
            .stream_output(&s.id, &proc.pid)
            .await
            .expect_err("after remove");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    // ----- write_stdin ---------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_write_stdin_then_send_succeeds() {
        // Arrange: stub backend installs a drain task on stdin_rx, so a
        // send always succeeds for the lifetime of the ProcessRecord.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        let result = mgr
            .write_stdin(&s.id, &proc.pid, bytes::Bytes::from_static(b"hello\n"))
            .await;

        // Assert
        assert!(result.is_ok(), "write_stdin should succeed: {result:?}");
    }

    #[tokio::test]
    async fn given_exec_when_write_empty_stdin_then_succeeds() {
        // Arrange: empty data is a valid no-op send — sometimes used as
        // a connectivity probe. The validator must not reject it.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        let result = mgr.write_stdin(&s.id, &proc.pid, bytes::Bytes::new()).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn given_unknown_pid_when_write_stdin_then_process_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let err = mgr
            .write_stdin(
                &s.id,
                "00000000-0000-0000-0000-000000000000",
                bytes::Bytes::from_static(b"x"),
            )
            .await
            .expect_err("unknown pid");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_pid_owned_by_other_sandbox_when_write_stdin_then_process_not_found() {
        // Arrange: tenant isolation regression guard — writing to a pid
        // that belongs to a different sandbox must fail as if the pid
        // didn't exist, not leak its existence.
        let mgr = build_manager(4);
        let s1 = mgr.create(create_req("alpine:1")).await.unwrap();
        let s2 = mgr.create(create_req("alpine:2")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s1.id.clone(),
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        let err = mgr
            .write_stdin(&s2.id, &proc.pid, bytes::Bytes::from_static(b"x"))
            .await
            .expect_err("cross-sandbox");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_malformed_pid_when_write_stdin_then_invalid_request() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act: 'z' is not hex.
        let err = mgr
            .write_stdin(&s.id, "not-hex-zzz", bytes::Bytes::from_static(b"x"))
            .await
            .expect_err("malformed pid");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    // ----- kill_process --------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_kill_process_then_subsequent_write_stdin_fails() {
        // Arrange: after a kill, the pid effectively no longer exists.
        // Any of the per-process RPCs must report ProcessNotFound. We
        // probe via write_stdin because it's the most observable side
        // effect (send-to-closed-channel becomes an error).
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        mgr.kill_process(&s.id, &proc.pid)
            .await
            .expect("kill_process");
        let err = mgr
            .write_stdin(&s.id, &proc.pid, bytes::Bytes::from_static(b"x"))
            .await
            .expect_err("write after kill");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_kill_already_done_when_called_again_then_process_not_found() {
        // Arrange: idempotency contract — once a pid is killed, subsequent
        // kills must be NotFound, not silently OK. This prevents callers
        // from masking real "I never knew about that pid" bugs as no-ops.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        mgr.kill_process(&s.id, &proc.pid).await.expect("first");

        // Act
        let err = mgr
            .kill_process(&s.id, &proc.pid)
            .await
            .expect_err("second");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_unknown_pid_when_kill_process_then_process_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let err = mgr
            .kill_process(&s.id, "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown pid");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_pid_owned_by_other_sandbox_when_kill_then_process_not_found() {
        // Arrange: tenant isolation regression — pid belongs to sandbox A,
        // sandbox B must not be able to kill it (or even confirm it exists).
        let mgr = build_manager(4);
        let s1 = mgr.create(create_req("alpine:1")).await.unwrap();
        let s2 = mgr.create(create_req("alpine:2")).await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s1.id,
                command: vec!["cat".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Act
        let err = mgr
            .kill_process(&s2.id, &proc.pid)
            .await
            .expect_err("cross-sandbox kill");

        // Assert
        assert!(matches!(err, ApiError::ProcessNotFound(_)));
    }

    #[tokio::test]
    async fn given_malformed_pid_when_kill_process_then_invalid_request() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let err = mgr
            .kill_process(&s.id, "not-hex-zzz")
            .await
            .expect_err("malformed pid");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    // ----- snapshots: error mapping at the manager boundary --------------

    #[tokio::test]
    async fn given_existing_sandbox_when_create_snapshot_then_returns_info() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let snap = mgr
            .create_snapshot(&s.id, "checkpoint")
            .await
            .expect("create_snapshot");

        // Assert
        assert_eq!(snap.snapshot_id.len(), 36);
        assert_eq!(snap.sandbox_id, s.id);
        assert_eq!(snap.label, "checkpoint");
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_create_snapshot_then_sandbox_not_found() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .create_snapshot("00000000-0000-0000-0000-000000000000", "x")
            .await
            .expect_err("unknown sandbox");

        // Assert: NOT SnapshotNotFound — the missing entity is the sandbox.
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_unknown_snapshot_when_restore_then_snapshot_not_found() {
        // Arrange: regression for the per-call error mapping override —
        // backend returns NotFound(snapshot_id) which the manager must
        // translate to SnapshotNotFound (not SandboxNotFound).
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let err = mgr
            .restore_snapshot(&s.id, "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown snapshot");

        // Assert
        assert!(
            matches!(err, ApiError::SnapshotNotFound(_)),
            "expected SnapshotNotFound, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn given_no_snapshots_when_list_then_returns_empty_vec() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine")).await.unwrap();

        // Act
        let snaps = mgr.list_snapshots(&s.id).await.unwrap();

        // Assert
        assert!(snaps.is_empty());
    }

    #[tokio::test]
    async fn given_malformed_sandbox_id_when_create_snapshot_then_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .create_snapshot("not-hex-zzz", "x")
            .await
            .expect_err("malformed");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }
}
