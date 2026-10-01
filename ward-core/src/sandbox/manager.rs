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
    StreamEvent, StreamEventKind,
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

/// Same translation as `backend_err`, but for lookups keyed by snapshot id:
/// a missing entity should surface as SnapshotNotFound, not SandboxNotFound.
fn snapshot_backend_err(e: BackendError) -> ApiError {
    match e {
        BackendError::NotFound(id) => ApiError::SnapshotNotFound(id),
        other => ApiError::Backend(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Per-sandbox tracking entry
// ---------------------------------------------------------------------------

struct SandboxEntry {
    egress: EgressProxy,
    timeout_handle: Option<tokio::task::JoinHandle<()>>,
    /// Held for the sandbox's whole lifetime. Dropping it (on removal)
    /// returns the slot to `create_semaphore` so a concurrent `create`
    /// waiting on the cap can proceed. Never read directly; the field
    /// exists for its `Drop` side effect.
    _creation_permit: tokio::sync::OwnedSemaphorePermit,
    /// The capability token minted for this sandbox at creation. Compared
    /// by `authorize()`; never returned by `get()`/`list()`.
    token: String,
}

// ---------------------------------------------------------------------------
// Per-process tracking entry
// ---------------------------------------------------------------------------

/// Number of buffered `StreamEvent`s the exit-reaping bridge (see
/// `SandboxManager::reap_on_exit`) will hold before backpressuring the
/// backend. Matches the stub backend's own scripted-output channel size.
const OUTPUT_BRIDGE_CAPACITY: usize = 16;

/// State held for each process spawned via exec/run. The output receiver is
/// wrapped in `Arc<Mutex<Option<...>>>` so the first `stream_output` call can
/// take it (a second call sees `None` and returns InvalidRequest) while the
/// Arc lets callers clone the handle out from under the `processes` RwLock
/// read guard before awaiting the inner lock. The stdin sender is plain
/// `Option<Sender>` because Sender is Clone — many concurrent WriteStdin
/// calls can share it. `None` represents a process that doesn't accept
/// stdin at all (real backend may produce these).
struct ProcessRecord {
    sandbox_id: String,
    output_rx: Arc<Mutex<Option<mpsc::Receiver<StreamEvent>>>>,
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
    /// Gates `create` so the cap in `max_sandboxes` holds even under
    /// concurrent calls. A permit is acquired before the backend creates
    /// the sandbox and held in the resulting `SandboxEntry` for its whole
    /// lifetime; checking `entries.len()` alone left a window between the
    /// check and the insert where concurrent creates could all pass.
    create_semaphore: Arc<tokio::sync::Semaphore>,
    /// SEC-020: snapshot of `Config::allow_host_mounts` taken at daemon
    /// startup. Stored on the manager so the security posture is read
    /// once and cannot mutate mid-process (in particular, a sandbox
    /// escape that touches `/proc/<pid>/environ` cannot widen it).
    allow_host_mounts: bool,
    /// Snapshot of `Config::network_backend`, same rationale as
    /// `allow_host_mounts`. Only the smoltcp backend has a datapath that
    /// enforces `EgressMode::Allowlist` (its per-flow guard in
    /// `ward-net`'s `Stack`); passt and gvproxy have no such enforcement
    /// point, so `create` rejects Allowlist up front for those backends
    /// rather than silently accepting a policy nothing checks.
    network_backend: crate::config::NetworkBackendChoice,
    /// Process records keyed by pid. Populated by exec/run; drained by
    /// stream_output. Lives for the lifetime of the manager; the leak
    /// is bounded by sandbox lifetime and cleaned up when the sandbox
    /// is removed.
    processes: Arc<RwLock<HashMap<String, ProcessRecord>>>,
}

impl SandboxManager {
    pub fn new(
        backend: Arc<dyn Backend>,
        broker: Arc<Broker>,
        max_sandboxes: usize,
        allow_host_mounts: bool,
        network_backend: crate::config::NetworkBackendChoice,
    ) -> Self {
        Self {
            backend,
            broker,
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_sandboxes,
            create_semaphore: Arc::new(tokio::sync::Semaphore::new(max_sandboxes)),
            allow_host_mounts,
            network_backend,
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

    /// Create a new sandbox. `presented_token` is only consulted when
    /// `req.from_snapshot` is set: it must match the token of the sandbox
    /// that owns the snapshot, so cloning a sandbox from its snapshot
    /// requires proving ownership of the source.
    pub async fn create(
        &self,
        req: crate::pb::CreateSandboxRequest,
        presented_token: &str,
    ) -> Result<PbSandboxInfo> {
        // Metrics: time the full create path (validation + backend boot +
        // bookkeeping). Recorded on every call regardless of outcome
        // because the failure rate matters as much as the success rate.
        let create_start = std::time::Instant::now();
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

        // Validate and convert bind mounts; the backend attaches them to the
        // microVM. (Previously mounts were silently dropped.)
        //
        // SEC-020: `self.allow_host_mounts` is the Config snapshot taken
        // at daemon startup, NOT a per-request env read. This makes the
        // security posture stable for the daemon's lifetime: a sandbox
        // escape that reads `/proc/<pid>/environ` and finds
        // `WARD_ALLOW_HOST_MOUNTS=1` cannot widen subsequent requests'
        // mount allowlist, because the validator reads the snapshot,
        // not the live env.
        let mut mounts = Vec::with_capacity(req.mounts.len());
        for m in &req.mounts {
            crate::validate::mount(&m.source, &m.target, m.readonly, self.allow_host_mounts)?;
            mounts.push(crate::protocol::Mount {
                source: m.source.clone(),
                target: m.target.clone(),
                readonly: m.readonly,
            });
        }

        // Validate volume references before they reach the backend.
        for vid in &req.volume_ids {
            crate::validate::entity_id(vid, "volume")?;
        }

        let id = uuid::Uuid::new_v4().to_string();

        let egress_policy = req.egress.map(pb_egress_to_protocol).unwrap_or_default();

        // SEC-ALLOWLIST: only the smoltcp backend's Stack enforces
        // EgressMode::Allowlist on the datapath (its per-flow guard).
        // NetworkBackendChoice::None has no datapath at all, so accepting
        // Allowlist for it would silently degrade to "policy set but
        // never checked": reject up front instead.
        if egress_policy.mode == crate::protocol::EgressMode::Allowlist
            && self.network_backend != crate::config::NetworkBackendChoice::Smoltcp
        {
            return Err(ApiError::InvalidRequest(format!(
                "egress mode Allowlist requires the smoltcp network backend \
                 (WARD_NETWORK_BACKEND=smoltcp); the configured backend \
                 ({:?}) has no datapath to enforce it",
                self.network_backend
            )));
        }

        let resources = req
            .resources
            .map(pb_resources_to_protocol)
            .unwrap_or_default();

        let comms = req.comms.map(pb_comms_to_protocol).unwrap_or_default();

        let opts = CreateOpts {
            image: req.image.clone(),
            mounts,
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

        // Cloning a sandbox from a snapshot requires the snapshot owner's
        // token, otherwise any caller could clone any other sandbox's
        // state by guessing or observing a snapshot id.
        if let Some(ref from_id) = opts.from_snapshot {
            let owner_id = self
                .backend
                .snapshot_owner(from_id)
                .await
                .map_err(snapshot_backend_err)?;
            self.authorize(&owner_id, presented_token).await?;
        }

        // Enforce the sandbox cap via a semaphore acquired up front, rather
        // than a check-then-insert on `entries.len()`. The old check and the
        // eventual `entries.write().await.insert(...)` were separated by the
        // `backend.create_sandbox` await, so concurrent calls could all pass
        // the check before any of them inserted, letting the count exceed
        // `max_sandboxes`. The permit is acquired before the backend call
        // and held for the sandbox's lifetime (see `SandboxEntry`), so the
        // cap holds under concurrency without serializing sandbox creation.
        let current = self.entries.read().await.len();
        let permit = Arc::clone(&self.create_semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::InvalidRequest(format!(
                    "sandbox limit reached ({}/{})",
                    current, self.max_sandboxes,
                ))
            })?;

        let mut info = self
            .backend
            .create_sandbox(id.clone(), &opts)
            .await
            .map_err(backend_err)?;

        let egress = EgressProxy::new(id.clone(), egress_policy);

        // Register a timeout watcher if requested. The watcher routes the
        // cleanup through `cleanup_sandbox_inner` (with from_timeout=true)
        // so the bookkeeping side effects, entry removal, process record
        // purge, broker deregistration, and the active-gauge decrement,
        // match the user-initiated remove path exactly. Without this the
        // gauge would never drop on timeout-driven shutdowns, gradually
        // overstating "active sandboxes" forever after the first timer
        // fires.
        let timeout_handle = if info.resources.timeout_seconds > 0 {
            let entries = Arc::clone(&self.entries);
            let broker = Arc::clone(&self.broker);
            let backend = Arc::clone(&self.backend);
            let processes = Arc::clone(&self.processes);
            let sandbox_id = id.clone();
            let secs = info.resources.timeout_seconds;
            Some(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                tracing::info!(sandbox_id = %sandbox_id, "sandbox timeout reached, removing");
                if let Err(e) = Self::cleanup_sandbox_inner(
                    &entries,
                    &broker,
                    &backend,
                    &processes,
                    &sandbox_id,
                    /* from_timeout = */ true,
                )
                .await
                {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "backend remove failed during timeout cleanup; local state still purged"
                    );
                }
            }))
        } else {
            None
        };

        // Capability token for this sandbox: a fresh random UUID, checked
        // by authorize() and never echoed back by get()/list().
        let token = uuid::Uuid::new_v4().to_string();

        self.entries.write().await.insert(
            id.clone(),
            SandboxEntry {
                egress,
                timeout_handle,
                _creation_permit: permit,
                token: token.clone(),
            },
        );

        // Register the comms policy with the broker so publish/subscribe
        // calls from this sandbox have something to match against. Done
        // after the entries-insert so we don't leak broker state if the
        // local registration fails — though entries.insert can't actually
        // fail here, the order keeps cleanup symmetric with remove().
        self.broker.register_sandbox(id, comms).await;

        // Metrics: success path. Failure paths (the early `?`s above)
        // already counted via the validate layer; this records the
        // sandbox-creation-actually-happened signal an operator wants.
        metrics::counter!("wardd_sandbox_create_total").increment(1);
        metrics::histogram!("wardd_sandbox_create_duration_seconds")
            .record(create_start.elapsed().as_secs_f64());
        metrics::gauge!("wardd_sandbox_active").increment(1.0);

        // The real token is returned exactly once, on creation. Set it
        // explicitly here rather than trusting the backend to have left
        // it empty, since this is the trust boundary that decides what
        // the caller sees.
        info.token = token;

        Ok(protocol_info_to_pb(info))
    }

    /// Retrieve info for an existing sandbox.
    pub async fn get(&self, id: &str) -> Result<PbSandboxInfo> {
        crate::validate::entity_id(id, "sandbox")?;
        let info = self.backend.get_sandbox(id).await.map_err(backend_err)?;
        Ok(protocol_info_to_pb(redact_token(info)))
    }

    /// List all sandboxes.
    pub async fn list(&self) -> Result<Vec<PbSandboxInfo>> {
        let infos = self.backend.list_sandboxes().await.map_err(backend_err)?;
        Ok(infos
            .into_iter()
            .map(redact_token)
            .map(protocol_info_to_pb)
            .collect())
    }

    /// Verify a presented capability token matches the one minted for
    /// `sandbox_id` at creation. Distinguishes an unknown sandbox from a
    /// wrong token so callers (and the gRPC layer's status mapping) can
    /// tell the two apart.
    pub async fn authorize(&self, sandbox_id: &str, token: &str) -> Result<()> {
        let entries = self.entries.read().await;
        let entry = entries
            .get(sandbox_id)
            .ok_or_else(|| ApiError::SandboxNotFound(sandbox_id.to_string()))?;
        if entry.token == token {
            Ok(())
        } else {
            Err(ApiError::PermissionDenied(format!(
                "token does not match sandbox {sandbox_id}"
            )))
        }
    }

    /// Return the egress decision log for a sandbox's proxy.
    pub async fn egress_log(&self, id: &str) -> Result<Vec<crate::egress::LogEntry>> {
        crate::validate::entity_id(id, "sandbox")?;
        let entries = self.entries.read().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        Ok(entry.egress.log_entries().await)
    }

    /// Remove a sandbox.
    pub async fn remove(&self, id: &str) -> Result<()> {
        crate::validate::entity_id(id, "sandbox")?;
        Self::cleanup_sandbox_inner(
            &self.entries,
            &self.broker,
            &self.backend,
            &self.processes,
            id,
            /* from_timeout = */ false,
        )
        .await
        .map_err(backend_err)
    }

    /// Shared cleanup path for user-initiated `remove()` and timer-driven
    /// expiry. Both must produce identical state: the entry leaves the
    /// `entries` map, the process records for that sandbox are dropped,
    /// the broker forgets the comms policy, the backend tears the
    /// sandbox down, and the `wardd_sandbox_active` gauge decrements.
    ///
    /// The `from_timeout` flag exists so the timer-driven path does not
    /// abort its own `JoinHandle`. Aborting the handle for the task we
    /// are currently running in would cancel the task at the next
    /// `.await`, leaving the cleanup half-applied. The user-driven path
    /// holds no such constraint and must abort the timer so a removed
    /// sandbox does not get torn down a second time when the timer
    /// later fires.
    async fn cleanup_sandbox_inner(
        entries: &Arc<RwLock<HashMap<String, SandboxEntry>>>,
        broker: &Arc<Broker>,
        backend: &Arc<dyn Backend>,
        processes: &Arc<RwLock<HashMap<String, ProcessRecord>>>,
        id: &str,
        from_timeout: bool,
    ) -> std::result::Result<(), BackendError> {
        let removed = if let Some(entry) = entries.write().await.remove(id) {
            if !from_timeout && let Some(handle) = entry.timeout_handle {
                handle.abort();
            }
            true
        } else {
            false
        };

        processes
            .write()
            .await
            .retain(|_, rec| rec.sandbox_id != id);
        broker.deregister_sandbox(id).await;

        let result = backend.remove_sandbox(id).await;
        if removed {
            metrics::counter!("wardd_sandbox_remove_total").increment(1);
            metrics::gauge!("wardd_sandbox_active").decrement(1.0);
        }
        result
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
            .map_err(snapshot_backend_err)
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
            output_rx: Arc::new(Mutex::new(handle.output_rx)),
            stdin_tx: handle.stdin_tx,
        };
        self.processes.write().await.insert(pid.clone(), record);

        Ok(ProcessInfo {
            pid,
            sandbox_id: req.sandbox_id,
            status: "running".to_string(),
        })
    }

    /// Forward backend output events to `stream_output`'s caller, and reap
    /// the `ProcessRecord` once the process signals completion. Runs
    /// detached from the `stream_output` call that spawned it, since a
    /// process that finishes on its own (without `kill_process` ever being
    /// called) otherwise has no path back to `self.processes` being
    /// cleaned up once its output has been consumed.
    async fn reap_on_exit(
        mut backend_rx: mpsc::Receiver<StreamEvent>,
        tx: mpsc::Sender<StreamEvent>,
        processes: Arc<RwLock<HashMap<String, ProcessRecord>>>,
        pid: String,
    ) {
        while let Some(event) = backend_rx.recv().await {
            let is_exit = event.kind == StreamEventKind::Exit;
            if tx.send(event).await.is_err() {
                // The receiving end (the stream_output caller) is gone;
                // nothing left to forward to.
                break;
            }
            if is_exit {
                break;
            }
        }
        // Reached on Exit, on the backend channel closing without an Exit
        // event, or on the forward failing above. `remove` is a no-op if
        // `kill_process` already dropped this record, so this is safe to
        // run unconditionally.
        processes.write().await.remove(&pid);
    }

    /// Take the output receiver for a previously-started process. Single-
    /// consumer: a second call returns InvalidRequest. The caller is
    /// expected to drain the channel and translate events into whatever
    /// stream type the transport needs.
    ///
    /// The backend's receiver is bridged through a manager-owned channel
    /// rather than handed back directly: `reap_on_exit` drains the backend
    /// side, forwards each event to the caller, and removes the
    /// `ProcessRecord` once it sees the Exit event (or the backend channel
    /// just closes). Bridging only starts here, once a caller has actually
    /// asked for output, rather than at `exec` time, so a process nobody
    /// has asked about yet stays fully addressable by write_stdin and
    /// kill_process.
    pub async fn stream_output(
        &self,
        sandbox_id: &str,
        pid: &str,
    ) -> Result<mpsc::Receiver<StreamEvent>> {
        crate::validate::entity_id(sandbox_id, "sandbox")?;
        crate::validate::entity_id(pid, "process")?;

        // Extract the output handle inside a short scope and drop the
        // `processes` read guard before the inner `Mutex` await below.
        // Holding a RwLock read guard across that await would block any
        // writer (exec, kill_process, the reaper above) for as long as the
        // inner lock takes to become available.
        let output_rx = {
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

            Arc::clone(&record.output_rx)
        };

        let backend_rx = output_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| ApiError::InvalidRequest("output stream already consumed".into()))?;

        let (tx, rx) = mpsc::channel::<StreamEvent>(OUTPUT_BRIDGE_CAPACITY);
        tokio::spawn(Self::reap_on_exit(
            backend_rx,
            tx,
            Arc::clone(&self.processes),
            pid.to_string(),
        ));
        Ok(rx)
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

        // Clone the sender inside a short scope and drop the `processes`
        // read guard before the inner `send` await below. Sender is Clone,
        // so this doesn't change who the channel talks to; it just avoids
        // holding the RwLock guard across an await that could block on the
        // receiving end for an unbounded time.
        let tx = {
            let guard = self.processes.read().await;
            let record = guard
                .get(pid)
                .ok_or_else(|| ApiError::ProcessNotFound(pid.to_string()))?;
            if record.sandbox_id != sandbox_id {
                return Err(ApiError::ProcessNotFound(pid.to_string()));
            }

            record
                .stdin_tx
                .clone()
                .ok_or_else(|| ApiError::InvalidRequest("process does not accept stdin".into()))?
        };

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
        // TODO(run-rpc): writing req.code into the guest via the vsock agent
        // channel is tracked in issue #9. Return Unimplemented so callers get
        // an honest error instead of a silent fake success.
        let _ = req;
        Err(ApiError::InvalidRequest(
            "Run RPC is not yet implemented; use Exec to run commands inside the sandbox"
                .to_string(),
        ))
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

/// Clear the capability token before a sandbox's info is returned by
/// `get()`/`list()`. Only the `create()` response carries the real token;
/// echoing it back here would let anyone who can query the API bypass the
/// check `authorize()` performs.
fn redact_token(mut info: crate::protocol::SandboxInfo) -> crate::protocol::SandboxInfo {
    info.token = String::new();
    info
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
        token: info.token,
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
    use pretty_assertions::assert_eq;

    /// Offline puller used by test backends so no network calls are made
    /// when manager tests create sandboxes.
    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl crate::backend::image::ImagePuller for FakePuller {
        async fn pull(
            &self,
            reference: &str,
            dest: &std::path::Path,
        ) -> crate::backend::Result<String> {
            std::fs::create_dir_all(dest.join("bin")).map_err(crate::backend::BackendError::Io)?;
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(format!("sha256:{hash:016x}"))
        }
    }

    /// Build a fresh SandboxManager pointed at a per-test data_dir.
    /// Injects a `FakePuller` so `create_sandbox` works offline.
    /// Leaks the TempDir intentionally: tokio's async fs API outlives any
    /// test-local scope, and the OS cleans /tmp on its own schedule.
    /// Default to allow_host_mounts=false so the cap path is exercised by
    /// the existing test corpus; per-test overrides go through a
    /// dedicated builder if they need the opt-in.
    fn build_manager(max_sandboxes: usize) -> Arc<SandboxManager> {
        build_manager_with_backend(max_sandboxes, crate::config::NetworkBackendChoice::Smoltcp)
    }

    /// Same as [`build_manager`], but with the network backend snapshot
    /// under test's control, for scenarios that depend on which backend
    /// is configured (e.g. the SEC-ALLOWLIST guard).
    fn build_manager_with_backend(
        max_sandboxes: usize,
        network_backend: crate::config::NetworkBackendChoice,
    ) -> Arc<SandboxManager> {
        use crate::backend::image::ImageStore;
        use crate::backend::krunvm::KrunvmBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        std::mem::forget(dir);
        let backend: Arc<dyn Backend> =
            Arc::new(KrunvmBackend::with_image_store_for_test(path, store));
        let broker = Arc::new(Broker::new());
        Arc::new(SandboxManager::new(
            backend,
            broker,
            max_sandboxes,
            /* allow_host_mounts = */ false,
            network_backend,
        ))
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
            .create(create_req("alpine:latest"), "")
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
            .create(create_req(""), "")
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
            .create(create_req("../../etc/passwd"), "")
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
        let err = mgr.create(req, "").await.expect_err("over-cap cpus");

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
        let err = mgr.create(req, "").await.expect_err("group without name");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_manager_at_capacity_when_create_then_returns_invalid_request_with_limit() {
        // Arrange: fill to capacity.
        let mgr = build_manager(2);
        mgr.create(create_req("alpine:1"), "").await.unwrap();
        mgr.create(create_req("alpine:2"), "").await.unwrap();

        // Act
        let err = mgr
            .create(create_req("alpine:3"), "")
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

    #[tokio::test]
    async fn given_manager_at_capacity_when_concurrent_creates_race_then_cap_never_exceeded() {
        // Arrange: cap of 2, more concurrent attempts than the cap allows so
        // they interleave around the `backend.create_sandbox` await that
        // separates the old check from the old insert.
        let mgr = build_manager(2);

        // Act: fire five creates at once from separate tasks.
        let mut handles = Vec::with_capacity(5);
        for i in 0..5 {
            let mgr = Arc::clone(&mgr);
            handles.push(tokio::spawn(async move {
                mgr.create(create_req(&format!("alpine:{i}")), "").await
            }));
        }
        let mut successes = 0;
        for handle in handles {
            if handle.await.expect("task should not panic").is_ok() {
                successes += 1;
            }
        }

        // Assert: the cap held under concurrency, and the manager's live
        // sandbox count matches the successes exactly (no orphaned entries).
        assert!(
            successes <= 2,
            "expected at most 2 successful creates, got {successes}"
        );
        let live = mgr.list().await.expect("list").len();
        assert_eq!(live, successes, "live sandbox count must match successes");
    }

    #[tokio::test]
    async fn given_egress_mode_allowlist_when_create_sandbox_then_no_longer_rejected() {
        // Arrange: Allowlist mode used to be hard-rejected at creation
        // because no datapath enforced it; the smoltcp backend now does.
        let mgr = build_manager(4);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            egress: Some(PbEgressPolicy {
                mode: PbEgressMode::Allowlist as i32,
                domains: vec!["api.example.com".into()],
            }),
            ..Default::default()
        };

        // Act
        let result = mgr.create(req, "").await;

        // Assert
        assert!(
            result.is_ok(),
            "Allowlist mode must no longer be rejected at creation: {result:?}"
        );
    }

    #[tokio::test]
    async fn given_none_backend_when_create_sandbox_with_allowlist_then_rejected() {
        // Arrange: NetworkBackendChoice::None has no datapath at all
        // (unlike smoltcp's Stack), so accepting an Allowlist policy for
        // it would silently degrade to "policy set but never checked".
        let mgr = build_manager_with_backend(4, crate::config::NetworkBackendChoice::None);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            egress: Some(PbEgressPolicy {
                mode: PbEgressMode::Allowlist as i32,
                domains: vec!["api.example.com".into()],
            }),
            ..Default::default()
        };

        // Act
        let err = mgr
            .create(req, "")
            .await
            .expect_err("the none backend cannot enforce Allowlist");

        // Assert
        match err {
            ApiError::InvalidRequest(msg) => {
                assert!(msg.contains("smoltcp"), "expected 'smoltcp' in: {msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ----- authorize -------------------------------------------------------

    #[tokio::test]
    async fn given_correct_token_when_authorize_then_ok() {
        // Arrange
        let mgr = build_manager(4);
        let info = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");

        // Act
        let result = mgr.authorize(&info.id, &info.token).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn given_wrong_token_when_authorize_then_permission_denied() {
        // Arrange
        let mgr = build_manager(4);
        let info = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");

        // Act
        let result = mgr.authorize(&info.id, "not-the-token").await;

        // Assert
        assert!(matches!(result, Err(ApiError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_authorize_then_not_found() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let result = mgr.authorize("does-not-exist", "x").await;

        // Assert
        assert!(matches!(result, Err(ApiError::SandboxNotFound(_))));
    }

    #[tokio::test]
    async fn given_valid_token_for_other_sandbox_when_authorize_then_permission_denied() {
        // Arrange: two distinct sandboxes, each with its own real token.
        let mgr = build_manager(4);
        let sandbox_a = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");
        let sandbox_b = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");

        // Act: present A's valid token against B's id.
        let result = mgr.authorize(&sandbox_b.id, &sandbox_a.token).await;

        // Assert
        assert!(matches!(result, Err(ApiError::PermissionDenied(_))));
    }

    // ----- token exposure --------------------------------------------------

    #[tokio::test]
    async fn given_create_when_returned_info_then_token_present() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let info = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");

        // Assert: the freshly minted token is non-empty and authorize()
        // accepts it for the sandbox it was minted for.
        assert!(!info.token.is_empty());
        assert!(mgr.authorize(&info.id, &info.token).await.is_ok());
    }

    #[tokio::test]
    async fn given_get_when_returned_info_then_token_redacted() {
        // Arrange: read the manager's own entry map, not create()'s
        // response, to confirm the sandbox holds a real, non-empty token
        // internally.
        let mgr = build_manager(4);
        let info = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create should succeed");
        let stored_token = mgr
            .entries
            .read()
            .await
            .get(&info.id)
            .expect("entry should exist")
            .token
            .clone();
        assert!(!stored_token.is_empty());

        // Act
        let fetched = mgr.get(&info.id).await.expect("get");

        // Assert: get() never echoes the stored token back.
        assert_eq!(fetched.token, "");
    }

    #[tokio::test]
    async fn given_list_when_returned_info_then_token_redacted() {
        // Arrange: create multiple sandboxes and confirm, via the
        // manager's own entry map, that each holds a real, non-empty
        // token internally.
        let mgr = build_manager(4);
        let a = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create a should succeed");
        let b = mgr
            .create(create_req("alpine:latest"), "")
            .await
            .expect("create b should succeed");
        {
            let entries = mgr.entries.read().await;
            assert!(!entries.get(&a.id).expect("entry a").token.is_empty());
            assert!(!entries.get(&b.id).expect("entry b").token.is_empty());
        }

        // Act
        let listed = mgr.list().await.expect("list");

        // Assert: list() never echoes any stored token back.
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|info| info.token.is_empty()));
    }

    // ----- from_snapshot token gate ---------------------------------------

    #[tokio::test]
    async fn given_from_snapshot_with_source_token_when_create_then_ok() {
        // Arrange: sandbox A and a snapshot of it.
        let mgr = build_manager(4);
        let a = mgr.create(create_req("alpine"), "").await.unwrap();
        let snap = mgr
            .create_snapshot(&a.id, "label")
            .await
            .expect("create_snapshot");
        let req2 = CreateSandboxRequest {
            from_snapshot: snap.snapshot_id,
            ..create_req("alpine")
        };

        // Act: present A's own token when creating from A's snapshot.
        let result = mgr.create(req2, &a.token).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn given_from_snapshot_without_source_token_when_create_then_permission_denied() {
        // Arrange: sandbox A and a snapshot of it.
        let mgr = build_manager(4);
        let a = mgr.create(create_req("alpine"), "").await.unwrap();
        let snap = mgr
            .create_snapshot(&a.id, "label")
            .await
            .expect("create_snapshot");
        let req2 = CreateSandboxRequest {
            from_snapshot: snap.snapshot_id,
            ..create_req("alpine")
        };

        // Act: no token presented for the snapshot's source sandbox.
        let result = mgr.create(req2, "").await;

        // Assert
        assert!(matches!(result, Err(ApiError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn given_from_snapshot_with_unknown_snapshot_id_when_create_then_snapshot_not_found() {
        // Arrange: a snapshot id nothing owns.
        let mgr = build_manager(4);
        let req = CreateSandboxRequest {
            from_snapshot: "does-not-exist".to_string(),
            ..create_req("alpine")
        };

        // Act
        let result = mgr.create(req, "x").await;

        // Assert: the snapshot lookup fails before any token check runs.
        assert!(matches!(result, Err(ApiError::SnapshotNotFound(_))));
    }

    // ----- get -----------------------------------------------------------

    #[tokio::test]
    async fn given_created_sandbox_when_get_by_id_then_returns_same_info() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();

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
        mgr.create(create_req("alpine:a"), "").await.unwrap();
        mgr.create(create_req("alpine:b"), "").await.unwrap();
        mgr.create(create_req("alpine:c"), "").await.unwrap();

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
    async fn given_sandbox_with_timeout_when_timer_fires_then_routed_through_shared_cleanup() {
        // Arrange: regression guard for the metrics-gauge leak. Pre-fix,
        // the timeout watcher called `backend.remove_sandbox` directly,
        // bypassing `cleanup_sandbox_inner` so the entries map never
        // shrank and `wardd_sandbox_active` never decremented. Now both
        // paths must agree.
        let mgr = build_manager(4);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            resources: Some(PbResourceLimits {
                timeout_seconds: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = mgr.create(req, "").await.expect("create");
        assert_eq!(mgr.entries.read().await.len(), 1, "precondition");

        // Act: wait past the timeout so the watcher fires.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        // Assert: the entries map shrank (proof the shared cleanup ran
        // rather than the old direct backend call), and the sandbox is
        // gone from the backend's perspective.
        assert!(
            mgr.entries.read().await.is_empty(),
            "entries map must shrink after timeout"
        );
        let err = mgr
            .get(&s.id)
            .await
            .expect_err("must be gone after timeout");
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_sandbox_removed_when_create_then_cap_slot_is_freed() {
        // Arrange: regression for cap-counter bookkeeping. Fill the cap,
        // remove one, then create one more.
        let mgr = build_manager(2);
        let s1 = mgr.create(create_req("alpine:1"), "").await.unwrap();
        let _s2 = mgr.create(create_req("alpine:2"), "").await.unwrap();

        // Act
        mgr.remove(&s1.id).await.unwrap();
        let s3 = mgr.create(create_req("alpine:3"), "").await;

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
    async fn given_run_rpc_when_called_then_returns_unimplemented() {
        // Run is stubbed until the vsock agent channel is wired (issue #9).
        // Every call returns InvalidRequest regardless of language or input.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

        let err = mgr
            .run(crate::pb::RunRequest {
                sandbox_id: s.id,
                language: "python".into(),
                code: "print('hi')".into(),
            })
            .await
            .expect_err("run should return unimplemented");

        match err {
            ApiError::InvalidRequest(msg) => {
                assert!(
                    msg.contains("not yet implemented"),
                    "unexpected error message: {msg}",
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ----- stream_output -------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_stream_output_then_drains_scripted_stdout_and_exit() {
        // Arrange: exec parks the receiver under the pid; we take it back
        // out and confirm the scripted stub events come through. The
        // first event is a Stdout line; the second is the Exit(0) marker.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
        let proc = mgr
            .exec(ExecRequest {
                sandbox_id: s.id.clone(),
                command: vec!["echo".into(), "hi".into()],
                ..Default::default()
            })
            .await
            .expect("exec");

        // Act: drain to completion without ever calling kill_process, so
        // the only way the ProcessRecord is reaped is via the Exit event.
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

        // Assert: a process that completes naturally, with kill_process
        // never called, is reaped from `processes` rather than left
        // behind. The bridging task drops its sender only after removing
        // the record, so observing the channel close (`after_close`) above
        // already guarantees this has happened.
        assert!(
            !mgr.processes.read().await.contains_key(&proc.pid),
            "ProcessRecord for a naturally-completed process must be reaped"
        );
    }

    #[tokio::test]
    async fn given_unknown_pid_when_stream_output_then_process_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s1 = mgr.create(create_req("alpine:1"), "").await.unwrap();
        let s2 = mgr.create(create_req("alpine:2"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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

    #[tokio::test]
    async fn given_stream_output_inner_lock_contended_when_called_then_processes_guard_not_held() {
        // Arrange: seed a process record whose inner `output_rx` mutex is
        // held by this task, standing in for a slow consumer on that lock.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
        let pid = "abc123".to_string();
        let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
        let output_rx = Arc::new(Mutex::new(Some(rx)));
        mgr.processes.write().await.insert(
            pid.clone(),
            ProcessRecord {
                sandbox_id: s.id.clone(),
                output_rx: Arc::clone(&output_rx),
                stdin_tx: None,
            },
        );
        let inner_guard = output_rx.lock().await;

        // Act: call stream_output while the inner mutex is held elsewhere.
        // If the outer `processes` read guard were still held while
        // awaiting the inner lock, a concurrent writer on `processes`
        // would be blocked for as long as this inner lock stays held.
        let mgr_task = Arc::clone(&mgr);
        let sandbox_id = s.id.clone();
        let pid_task = pid.clone();
        let stream_task =
            tokio::spawn(async move { mgr_task.stream_output(&sandbox_id, &pid_task).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Assert: a concurrent write-lock acquisition on `processes` (what
        // exec/kill_process need) is not blocked.
        let write_attempt =
            tokio::time::timeout(std::time::Duration::from_millis(200), mgr.processes.write())
                .await;
        assert!(
            write_attempt.is_ok(),
            "processes write lock was blocked by stream_output's in-flight inner lock wait"
        );
        drop(write_attempt);

        drop(inner_guard);
        stream_task
            .await
            .expect("task should not panic")
            .expect("stream_output should succeed once the inner lock is free");
    }

    // ----- write_stdin ---------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_write_stdin_then_send_succeeds() {
        // Arrange: stub backend installs a drain task on stdin_rx, so a
        // send always succeeds for the lifetime of the ProcessRecord.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s1 = mgr.create(create_req("alpine:1"), "").await.unwrap();
        let s2 = mgr.create(create_req("alpine:2"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

        // Act: 'z' is not hex.
        let err = mgr
            .write_stdin(&s.id, "not-hex-zzz", bytes::Bytes::from_static(b"x"))
            .await
            .expect_err("malformed pid");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_write_stdin_send_blocked_when_called_then_processes_guard_not_held() {
        // Arrange: seed a process record whose stdin channel already holds
        // one buffered message, so the next send blocks until this task
        // drains it, standing in for a slow-to-consume backend.
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
        let pid = "def456".to_string();
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<bytes::Bytes>(1);
        stdin_tx
            .send(bytes::Bytes::from_static(b"seed"))
            .await
            .expect("seed send");
        mgr.processes.write().await.insert(
            pid.clone(),
            ProcessRecord {
                sandbox_id: s.id.clone(),
                output_rx: Arc::new(Mutex::new(None)),
                stdin_tx: Some(stdin_tx),
            },
        );

        // Act: write_stdin's inner send now blocks on the full channel
        // until this task drains `stdin_rx`. If the outer `processes` read
        // guard were still held while awaiting that send, a concurrent
        // writer on `processes` would be blocked for as long as the send
        // stays pending.
        let mgr_task = Arc::clone(&mgr);
        let sandbox_id = s.id.clone();
        let pid_task = pid.clone();
        let write_task = tokio::spawn(async move {
            mgr_task
                .write_stdin(&sandbox_id, &pid_task, bytes::Bytes::from_static(b"data"))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Assert: a concurrent write-lock acquisition on `processes` is not
        // blocked by write_stdin's in-flight, still-pending send.
        let write_attempt =
            tokio::time::timeout(std::time::Duration::from_millis(200), mgr.processes.write())
                .await;
        assert!(
            write_attempt.is_ok(),
            "processes write lock was blocked by write_stdin's in-flight send"
        );
        drop(write_attempt);

        // Cleanup: drain the channel so write_stdin's send can complete.
        stdin_rx.recv().await;
        write_task
            .await
            .expect("task should not panic")
            .expect("write_stdin should succeed once the channel drains");
    }

    // ----- kill_process --------------------------------------------------

    #[tokio::test]
    async fn given_exec_when_kill_process_then_subsequent_write_stdin_fails() {
        // Arrange: after a kill, the pid effectively no longer exists.
        // Any of the per-process RPCs must report ProcessNotFound. We
        // probe via write_stdin because it's the most observable side
        // effect (send-to-closed-channel becomes an error).
        let mgr = build_manager(4);
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s1 = mgr.create(create_req("alpine:1"), "").await.unwrap();
        let s2 = mgr.create(create_req("alpine:2"), "").await.unwrap();
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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
        let s = mgr.create(create_req("alpine"), "").await.unwrap();

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
