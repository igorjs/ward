// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! SandboxManager coordinates the backend, egress proxies, and timeout tracking.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::backend::krunvm::KrunvmBackend;
use crate::egress::EgressProxy;
use crate::pb::{
    ExecRequest, ProcessInfo, RunRequest, SandboxInfo as PbSandboxInfo, SandboxStatus,
};
use crate::protocol::{
    ApiError, CommunicationMode, CommunicationPolicy, CreateOpts, EgressPolicy, ResourceLimits,
};

type Result<T> = std::result::Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Per-sandbox tracking entry
// ---------------------------------------------------------------------------

struct SandboxEntry {
    #[allow(dead_code)]
    egress: EgressProxy,
    timeout_handle: Option<tokio::task::JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Coordinates sandbox lifecycle across the backend and supporting subsystems.
pub struct SandboxManager {
    backend: Arc<KrunvmBackend>,
    entries: Arc<RwLock<HashMap<String, SandboxEntry>>>,
    /// Maximum concurrent sandboxes. Prevents resource exhaustion from unbounded creation.
    max_sandboxes: usize,
}

impl SandboxManager {
    pub fn new(backend: Arc<KrunvmBackend>, max_sandboxes: usize) -> Self {
        Self {
            backend,
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_sandboxes,
        }
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
            comms,
        };

        let info = self
            .backend
            .create_sandbox(id.clone(), &opts)
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))?;

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
            id,
            SandboxEntry {
                egress,
                timeout_handle,
            },
        );

        Ok(protocol_info_to_pb(info))
    }

    /// Retrieve info for an existing sandbox.
    pub async fn get(&self, id: &str) -> Result<PbSandboxInfo> {
        crate::validate::entity_id(id, "sandbox")?;
        let info = self
            .backend
            .get_sandbox(id)
            .await
            .map_err(|e| ApiError::SandboxNotFound(e.to_string()))?;
        Ok(protocol_info_to_pb(info))
    }

    /// List all sandboxes.
    pub async fn list(&self) -> Result<Vec<PbSandboxInfo>> {
        let infos = self
            .backend
            .list_sandboxes()
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))?;
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

        self.backend
            .remove_sandbox(id)
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))
    }

    /// Return the number of active sandboxes.
    pub async fn count(&self) -> Result<usize> {
        self.backend
            .count()
            .await
            .map_err(|e| ApiError::Backend(e.to_string()))
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
            .map_err(|e| ApiError::Backend(e.to_string()))?;

        Ok(ProcessInfo {
            pid: handle.pid,
            sandbox_id: req.sandbox_id,
            status: "running".to_string(),
        })
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
