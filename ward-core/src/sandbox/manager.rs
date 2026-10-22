// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! SandboxManager coordinates the backend, egress proxies, and timeout tracking.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, broadcast, mpsc, watch};

use crate::backend::image::ImageConfig;
use crate::backend::{Backend, BackendError, ExecMode, ProcessControl, ProcessHandle};
use crate::comms::Broker;
use crate::egress::EgressProxy;
use crate::pb::{
    ExecRequest, ProcessInfo, RunRequest, SandboxInfo as PbSandboxInfo, SandboxStatus,
};
use crate::protocol::{
    ApiError, CommunicationMode, CommunicationPolicy, CreateOpts, EgressPolicy, OutputChunk,
    RawStream, ResourceLimits, StreamEvent, StreamEventKind,
};
use crate::sandbox::copy;
use crate::sandbox::events::{EventAction, EventLog, SandboxEvent};
use crate::sandbox::launch::{self, Launch, Overrides};
use crate::sandbox::store::{MetaStore, SandboxMeta, hash_token, token_hash_eq};

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
        BackendError::InvalidArgument(message) => ApiError::InvalidRequest(message),
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
    /// The persisted record, including the hash of the capability token
    /// minted at creation. `authorize()` compares against that hash; the
    /// token itself is never kept.
    meta: SandboxMeta,
}

/// Holds a sandbox name while its creation is in flight, so two concurrent
/// creates cannot both claim it. Released on drop, whether the create
/// succeeded or failed.
struct NameReservation {
    pending: Arc<std::sync::Mutex<HashSet<String>>>,
    name: String,
}

impl NameReservation {
    /// `None` when another create already holds `name`.
    fn claim(pending: &Arc<std::sync::Mutex<HashSet<String>>>, name: &str) -> Option<Self> {
        let claimed = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string());
        claimed.then(|| Self {
            pending: Arc::clone(pending),
            name: name.to_string(),
        })
    }
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.name);
    }
}

fn name_in_use(name: &str, id: Option<&str>) -> ApiError {
    ApiError::NameInUse {
        name: name.to_string(),
        id: id.map(str::to_string),
    }
}

/// Docker's wording when neither the request nor the image names a command.
const NO_COMMAND_MESSAGE: &str = "no command specified";

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

/// Bytes of main process output kept per sandbox for `logs` and attach replay.
const MAIN_OUTPUT_CAP: usize = 1024 * 1024;

/// Bookkeeping bytes charged against the cap for every retained chunk, so
/// many tiny writes cannot hold far more memory than the cap suggests.
const OUTPUT_CHUNK_OVERHEAD: usize = 64;

/// Largest retained chunk a same millisecond write may still be merged into,
/// which bounds the copy each write pays.
const OUTPUT_MERGE_LIMIT: usize = 4096;

/// Live chunks a slow subscriber may lag behind before it loses the oldest.
const MAIN_OUTPUT_LIVE_CHUNKS: usize = 64;

/// Bounded, memory only copy of a main process's output. Keeps the newest
/// `cap` bytes, with the stream of each chunk, and fans new chunks out to live
/// subscribers.
struct OutputBuffer {
    cap: usize,
    state: std::sync::Mutex<BufferState>,
}

struct BufferState {
    chunks: VecDeque<OutputChunk>,
    len: usize,
    live: broadcast::Sender<OutputChunk>,
}

impl OutputBuffer {
    fn new(cap: usize) -> Self {
        let (live, _) = broadcast::channel(MAIN_OUTPUT_LIVE_CHUNKS);
        Self {
            cap,
            state: std::sync::Mutex::new(BufferState {
                chunks: VecDeque::new(),
                len: 0,
                live,
            }),
        }
    }

    fn push(&self, chunk: OutputChunk) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        // Same stream and same millisecond: one chunk, so a burst of tiny
        // writes does not pay the per chunk overhead each time.
        let merged = match state.chunks.back_mut() {
            Some(back)
                if back.stream == chunk.stream
                    && same_millisecond(back.at, chunk.at)
                    && back.bytes.len() < OUTPUT_MERGE_LIMIT =>
            {
                let mut joined = Vec::with_capacity(back.bytes.len() + chunk.bytes.len());
                joined.extend_from_slice(&back.bytes);
                joined.extend_from_slice(&chunk.bytes);
                back.bytes = bytes::Bytes::from(joined);
                state.len += chunk.bytes.len();
                true
            }
            _ => false,
        };
        if !merged {
            state.len += chunk.bytes.len() + OUTPUT_CHUNK_OVERHEAD;
            state.chunks.push_back(chunk.clone());
        }
        while state.len > self.cap {
            let excess = state.len - self.cap;
            let Some(front) = state.chunks.front_mut() else {
                break;
            };
            if front.bytes.len() > excess {
                front.bytes = front.bytes.slice(excess..);
                state.len -= excess;
            } else {
                state.len -= front.bytes.len() + OUTPUT_CHUNK_OVERHEAD;
                state.chunks.pop_front();
            }
        }
        // No subscribers is the normal case before a client attaches.
        let _ = state.live.send(chunk);
    }

    /// Everything buffered so far plus a receiver for what follows. Both are
    /// taken under one lock, so no chunk is missed or repeated.
    fn subscribe(&self) -> (Vec<OutputChunk>, broadcast::Receiver<OutputChunk>) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            state.chunks.iter().cloned().collect(),
            state.live.subscribe(),
        )
    }
}

fn same_millisecond(a: chrono::DateTime<chrono::Utc>, b: chrono::DateTime<chrono::Utc>) -> bool {
    a.timestamp_millis() == b.timestamp_millis()
}

/// The process the daemon spawned from a sandbox's stored command.
struct MainProcess {
    pid: String,
    output: Arc<OutputBuffer>,
    /// Held so stdin stays open while no client is attached. Taken to send EOF.
    stdin_tx: Option<mpsc::Sender<bytes::Bytes>>,
    control_tx: Option<mpsc::Sender<ProcessControl>>,
    /// Raw `wait(2)` status, set once the process ends.
    exit: watch::Receiver<Option<i32>>,
}

/// Bit position of the exit code inside a raw wait(2) status.
const WAIT_STATUS_EXIT_SHIFT: i32 = 8;
const WAIT_STATUS_SIGNAL_MASK: i32 = 0x7f;
const WAIT_STATUS_EXIT_MASK: i32 = 0xff;
/// Shells report a process killed by signal N as 128 + N.
const SIGNAL_EXIT_BASE: i32 = 128;
/// Exit code Docker reports when a command cannot be run.
const EXEC_FAILED_EXIT: i32 = 126;

/// Seconds a stopping sandbox's main process gets to end after SIGTERM.
pub const STOP_GRACE_SECONDS: u64 = 10;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
/// Highest signal number Linux accepts (the last real-time signal).
const MAX_SIGNAL: i32 = 64;
/// The pid `kill_process` accepts for the sandbox's main process.
pub const MAIN_PROCESS_PID: &str = "0";

/// How often `wait` looks at the recorded state.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(100);
const BYTES_PER_MIB: u64 = 1024 * 1024;

/// One reading of a running sandbox's resource use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxStats {
    pub cpu_nanos: u64,
    pub online_cpus: u32,
    pub mem_used_bytes: u64,
    pub mem_limit_bytes: u64,
    pub pids: u32,
    pub read_at: std::time::SystemTime,
}

/// Turn a raw `wait(2)` status into the exit code a shell would report.
pub(crate) fn decode_wait_status(raw: i32) -> i32 {
    match raw & WAIT_STATUS_SIGNAL_MASK {
        0 => (raw >> WAIT_STATUS_EXIT_SHIFT) & WAIT_STATUS_EXIT_MASK,
        signal => SIGNAL_EXIT_BASE + signal,
    }
}

/// Channels to a running process.
pub struct ProcessIo {
    pub stdin: Option<mpsc::Sender<bytes::Bytes>>,
    pub control: Option<mpsc::Sender<ProcessControl>>,
}

/// The output an event carries, if any.
pub(crate) fn event_chunk(event: &StreamEvent) -> Option<OutputChunk> {
    let line = || bytes::Bytes::from(format!("{}\n", event.line));
    let at = chrono::Utc::now();
    match &event.kind {
        StreamEventKind::Data { stream, bytes } => Some(OutputChunk {
            stream: *stream,
            bytes: bytes::Bytes::copy_from_slice(bytes),
            at,
        }),
        StreamEventKind::Stdout => Some(OutputChunk {
            stream: RawStream::Stdout,
            bytes: line(),
            at,
        }),
        StreamEventKind::Stderr => Some(OutputChunk {
            stream: RawStream::Stderr,
            bytes: line(),
            at,
        }),
        StreamEventKind::Exit => None,
    }
}

/// Raw `wait(2)` status of an `Exit` event, built from the exit code when the
/// backend reports no raw status.
pub(crate) fn event_wait_status(event: &StreamEvent) -> Option<i32> {
    event
        .wait_status
        .or(event.exit_code.map(|code| code << WAIT_STATUS_EXIT_SHIFT))
}

/// Copy a main process's events into its buffer, then publish its raw wait
/// status when it ends.
async fn pump_main_output(
    mut events: mpsc::Receiver<StreamEvent>,
    output: Arc<OutputBuffer>,
    exit: watch::Sender<Option<i32>>,
) {
    while let Some(event) = events.recv().await {
        if let Some(chunk) = event_chunk(&event) {
            output.push(chunk);
        } else {
            exit.send_replace(event_wait_status(&event));
            return;
        }
    }
}

async fn persist_meta(store: Option<&MetaStore>, meta: SandboxMeta) -> Result<()> {
    let Some(store) = store.cloned() else {
        return Ok(());
    };
    let id = meta.id.clone();
    tokio::task::spawn_blocking(move || store.save(&meta))
        .await
        .map_err(|e| ApiError::Internal(format!("persist task for sandbox {id}: {e}")))?
        .map_err(|e| ApiError::Internal(format!("cannot persist sandbox {id}: {e}")))
}

/// Write the new state to disk, then adopt it in memory, so a failed write
/// leaves the entry in its old state.
async fn commit_meta(
    store: Option<&MetaStore>,
    entry: &mut SandboxEntry,
    status: crate::protocol::SandboxStatus,
    exit_code: Option<i32>,
) -> Result<()> {
    let mut next = entry.meta.clone();
    stamp_times(&mut next, &status, std::time::SystemTime::now());
    next.status = status;
    next.exit_code = exit_code;
    persist_meta(store, next.clone()).await?;
    entry.meta = next;
    Ok(())
}

/// Record the start and finish times a move to `next` implies.
fn stamp_times(
    meta: &mut SandboxMeta,
    next: &crate::protocol::SandboxStatus,
    now: std::time::SystemTime,
) {
    use crate::protocol::SandboxStatus as State;
    match next {
        State::Running
            if meta.status != State::Running && !meta.options.spec.command.is_empty() =>
        {
            meta.started_at = Some(now);
        }
        State::Exited => meta.finished_at = Some(now),
        State::Stopped => {
            meta.finished_at.get_or_insert(now);
        }
        _ => {}
    }
}

/// Wait for the main process to end. `None` waits without limit.
async fn ended_within(
    exit: &mut watch::Receiver<Option<i32>>,
    grace: Option<std::time::Duration>,
) -> bool {
    let ended = exit.wait_for(Option::is_some);
    match grace {
        Some(grace) => matches!(tokio::time::timeout(grace, ended).await, Ok(Ok(_))),
        None => ended.await.is_ok(),
    }
}

/// The shared handles a background task needs to tear a sandbox down and
/// report what happened to it.
#[derive(Clone)]
struct Teardown {
    entries: Arc<RwLock<HashMap<String, SandboxEntry>>>,
    mains: Arc<RwLock<HashMap<String, MainProcess>>>,
    broker: Arc<Broker>,
    backend: Arc<dyn Backend>,
    processes: Arc<RwLock<HashMap<String, ProcessRecord>>>,
    events: EventLog,
}

fn event_with_code(action: EventAction, meta: &SandboxMeta, code: Option<i32>) -> SandboxEvent {
    let event = SandboxEvent::new(action, meta);
    match code {
        Some(code) => event.with_exit_code(code),
        None => event,
    }
}

/// Mark a sandbox `exited` once its main process `pid` ends on its own. A
/// stop in progress records the outcome itself, and a newer run owns the
/// state, so both are left alone.
async fn mark_exited_on_main_end(
    teardown: Teardown,
    store: Option<MetaStore>,
    id: String,
    pid: String,
    mut exit: watch::Receiver<Option<i32>>,
    auto_remove: bool,
) {
    let Ok(raw) = exit.wait_for(Option::is_some).await.map(|status| *status) else {
        return;
    };
    let Some(code) = raw.map(decode_wait_status) else {
        return;
    };
    {
        let mut entries = teardown.entries.write().await;
        let current = teardown.mains.read().await.get(&id).map(|m| m.pid == pid);
        let Some(entry) = entries.get_mut(&id) else {
            return;
        };
        if current != Some(true) || entry.meta.status != crate::protocol::SandboxStatus::Running {
            return;
        }
        match commit_meta(
            store.as_ref(),
            entry,
            crate::protocol::SandboxStatus::Exited,
            Some(code),
        )
        .await
        {
            Ok(()) => {
                tracing::info!(sandbox_id = %id, code, "main process ended, sandbox exited");
                let died = SandboxEvent::new(EventAction::Die, &entry.meta).with_exit_code(code);
                teardown.events.emit(died);
            }
            Err(e) => tracing::warn!(sandbox_id = %id, error = %e, "cannot record the exit"),
        }
    }
    // The status is published and attached readers hold their own buffer
    // subscriptions, so removing now cannot cut their output short.
    if auto_remove {
        auto_remove_ended(&teardown, &id, &pid).await;
    }
}

/// Remove sandbox `id` if it is still exited from the run that `pid` was.
/// A start since the exit was recorded owns the sandbox now, so it stays.
async fn auto_remove_ended(teardown: &Teardown, id: &str, pid: &str) {
    let removed = {
        let mut entries = teardown.entries.write().await;
        let exited = entries
            .get(id)
            .is_some_and(|entry| entry.meta.status == crate::protocol::SandboxStatus::Exited);
        let same_run = teardown
            .mains
            .read()
            .await
            .get(id)
            .is_some_and(|m| m.pid == pid);
        if !(exited && same_run) {
            tracing::info!(sandbox_id = %id, "sandbox started again, keeping it");
            return;
        }
        SandboxManager::detach_entry(entries.remove(id), /* from_timeout = */ false)
    };
    let result = SandboxManager::finish_cleanup(removed, teardown, id).await;
    match result {
        Ok(()) => {
            tracing::info!(sandbox_id = %id, "auto-removed sandbox after its main process ended")
        }
        Err(e) => tracing::warn!(sandbox_id = %id, error = %e, "cannot auto-remove sandbox"),
    }
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
    /// Main process of each sandbox that has a command, keyed by sandbox id.
    mains: Arc<RwLock<HashMap<String, MainProcess>>>,
    /// Bytes kept per main process output buffer.
    output_cap: usize,
    /// How long `stop` waits for the main process after SIGTERM.
    stop_grace: std::time::Duration,
    /// On-disk sandbox records. `None` keeps everything in memory.
    store: Option<MetaStore>,
    /// Names claimed by creates that have not yet inserted their entry.
    pending_names: Arc<std::sync::Mutex<HashSet<String>>>,
    events: EventLog,
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
            mains: Arc::new(RwLock::new(HashMap::new())),
            output_cap: MAIN_OUTPUT_CAP,
            stop_grace: std::time::Duration::from_secs(STOP_GRACE_SECONDS),
            store: None,
            pending_names: Arc::new(std::sync::Mutex::new(HashSet::new())),
            events: EventLog::default(),
        }
    }

    /// Lifecycle events of every sandbox, for `Events` readers.
    pub fn events(&self) -> &EventLog {
        &self.events
    }

    fn teardown(&self) -> Teardown {
        Teardown {
            entries: Arc::clone(&self.entries),
            mains: Arc::clone(&self.mains),
            broker: Arc::clone(&self.broker),
            backend: Arc::clone(&self.backend),
            processes: Arc::clone(&self.processes),
            events: self.events.clone(),
        }
    }

    /// Report `action` for sandbox `id`. A sandbox that is gone has nothing
    /// to report.
    async fn emit_event(&self, id: &str, action: EventAction, exit_code: Option<i32>) {
        // Emitting under the lock orders the event before a later removal.
        let entries = self.entries.read().await;
        if let Some(entry) = entries.get(id) {
            self.events
                .emit(event_with_code(action, &entry.meta, exit_code));
        }
    }

    /// Report `action` from a record taken earlier, for a sandbox that may
    /// have been removed meanwhile.
    async fn emit_for(&self, meta: &SandboxMeta, action: EventAction, exit_code: Option<i32>) {
        let _entries = self.entries.read().await;
        self.events.emit(event_with_code(action, meta, exit_code));
    }

    /// Report `die` for a sandbox that was running and no longer is.
    async fn emit_die_if_was_running(
        &self,
        meta: &SandboxMeta,
        before: &crate::protocol::SandboxStatus,
        exit_code: i32,
    ) {
        // An exited sandbox already reported its `die`.
        if *before == crate::protocol::SandboxStatus::Running {
            self.emit_for(meta, EventAction::Die, Some(exit_code)).await;
        }
    }

    /// Record that the main process could not start and report its `die`.
    async fn fail_main_start(&self, id: &str) {
        self.mark_failed(id).await;
        self.emit_event(id, EventAction::Die, Some(EXEC_FAILED_EXIT))
            .await;
    }

    /// Persist sandbox records under `data_dir` and reload them with
    /// [`restore`](Self::restore).
    pub fn with_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.store = Some(MetaStore::new(&data_dir));
        self
    }

    #[cfg(all(test, not(feature = "krunvm")))]
    fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    #[cfg(all(test, not(feature = "krunvm")))]
    fn with_stop_grace(mut self, grace: std::time::Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// Reload sandboxes persisted by an earlier daemon run. Running ones
    /// come back stopped, since no VM outlives the daemon.
    pub async fn restore(&self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let metas = match tokio::task::spawn_blocking(move || store.load_all()).await {
            Ok(metas) => metas,
            Err(e) => {
                tracing::warn!(error = %e, "sandbox reload task failed");
                return;
            }
        };
        for meta in metas {
            self.adopt_persisted(meta).await;
        }
    }

    /// Record a state change and write it to disk.
    pub async fn record_state(
        &self,
        id: &str,
        status: crate::protocol::SandboxStatus,
        exit_code: Option<i32>,
    ) -> Result<()> {
        // The write lock is held across the save so a concurrent removal
        // cannot delete the dir and then see this write recreate it.
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        self.commit(entry, status, exit_code).await
    }

    /// Write the new state to disk, then adopt it in memory, so a failed
    /// write leaves the entry in its old state.
    async fn commit(
        &self,
        entry: &mut SandboxEntry,
        status: crate::protocol::SandboxStatus,
        exit_code: Option<i32>,
    ) -> Result<()> {
        commit_meta(self.store.as_ref(), entry, status, exit_code).await
    }

    async fn persist(&self, meta: SandboxMeta) -> Result<()> {
        persist_meta(self.store.as_ref(), meta).await
    }

    /// Bring one reloaded record back into the manager, or delete it if
    /// its wall-time limit has already passed or it is an auto-remove
    /// sandbox that is no longer running.
    async fn adopt_persisted(&self, meta: SandboxMeta) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let id = meta.id.clone();
        let created_at = meta.created_at;
        let timeout_seconds = meta.options.resources.timeout_seconds;
        let limit = std::time::Duration::from_secs(timeout_seconds);
        let now = std::time::SystemTime::now();
        let expires_at = (timeout_seconds > 0).then(|| created_at + limit);
        let remaining = expires_at.map(|at| at.duration_since(now).ok());
        let expired = remaining == Some(None);
        let finished_auto_remove =
            meta.options.spec.auto_remove && meta.status != crate::protocol::SandboxStatus::Running;
        if expired || finished_auto_remove {
            let reason = if expired {
                "past its timeout"
            } else {
                "that was set to auto-remove"
            };
            tracing::info!(sandbox_id = %id, "removing reloaded sandbox {reason}");
            let doomed = id.clone();
            match tokio::task::spawn_blocking(move || store.delete(&doomed)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(sandbox_id = %id, error = %e, "cannot remove reloaded sandbox");
                }
                Err(e) => {
                    tracing::warn!(sandbox_id = %id, error = %e, "reloaded sandbox removal task failed");
                }
            }
            return;
        }
        let remaining = remaining.flatten();

        let Ok(permit) = Arc::clone(&self.create_semaphore).try_acquire_owned() else {
            tracing::warn!(sandbox_id = %id, "skipping reloaded sandbox: sandbox limit reached");
            return;
        };
        let info = crate::protocol::SandboxInfo {
            id: id.clone(),
            status: meta.status.clone(),
            image: meta.options.image.clone(),
            created_at,
            ip_address: None,
            resources: meta.options.resources.clone(),
            expires_at,
            token: String::new(),
            spec: meta.options.spec.clone(),
            exit_code: meta.exit_code,
            started_at: meta.started_at,
            finished_at: meta.finished_at,
        };
        if let Err(e) = self
            .backend
            .adopt_sandbox(info, meta.options.clone(), store.rootfs_path(&id))
            .await
        {
            tracing::warn!(sandbox_id = %id, error = %e, "skipping reloaded sandbox: backend refused it");
            return;
        }

        let timeout_handle = remaining.map(|left| self.spawn_timeout(&id, left));
        let egress = EgressProxy::new(id.clone(), meta.options.egress.clone());
        self.entries.write().await.insert(
            id.clone(),
            SandboxEntry {
                egress,
                timeout_handle,
                _creation_permit: permit,
                meta,
            },
        );
        // No broker registration: a reloaded sandbox is not running, and
        // `start` registers its stored policy.
        tracing::info!(sandbox_id = %id, "reloaded sandbox from disk");
    }

    /// Borrow the shared broker. Useful for the gRPC layer which needs
    /// to call publish/subscribe/log without going through the manager.
    pub fn broker(&self) -> Arc<Broker> {
        Arc::clone(&self.broker)
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// The main process, working directory and environment for `req`, taking
    /// the image config into account when the request asks for it.
    async fn resolve_launch(&self, req: &crate::pb::CreateSandboxRequest) -> Result<Launch> {
        // A clone keeps the source's command, so its image is not consulted.
        let consult_image = req.use_image_config && req.from_snapshot.is_empty();
        let image = if consult_image && !req.image.is_empty() {
            self.backend
                .image_config(&req.image)
                .await
                .map_err(backend_err)?
        } else {
            ImageConfig::default()
        };
        // Without the opt in, the request alone decides, as before.
        let entrypoint = req
            .use_image_config
            .then_some(req.entrypoint.as_deref())
            .flatten();
        let launch = launch::resolve(
            &image,
            Overrides {
                entrypoint,
                args: &req.command,
                workdir: &req.workdir,
                env: &req.env,
            },
        );
        if !launch.user.is_empty() {
            tracing::warn!(
                image = %req.image,
                user = %launch.user,
                "image user is not applied; commands run as the guest default"
            );
        }
        if req.require_command && launch.command.is_empty() {
            return Err(ApiError::InvalidRequest(NO_COMMAND_MESSAGE.to_string()));
        }
        Ok(launch)
    }

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

        let name_reservation = self.reserve_name(&req.name).await?;

        let id = uuid::Uuid::new_v4().to_string();

        let egress_policy = req
            .egress
            .clone()
            .map(pb_egress_to_protocol)
            .unwrap_or_default();

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

        // Cloning a sandbox from a snapshot requires the snapshot owner's
        // token, otherwise any caller could clone any other sandbox's
        // state by guessing or observing a snapshot id.
        if !req.from_snapshot.is_empty() {
            let owner_id = self
                .backend
                .snapshot_owner(&req.from_snapshot)
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

        // Last, so a rejected request never pulls an image.
        let launch = self.resolve_launch(&req).await?;

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
            env: launch.env,
            from_snapshot: if req.from_snapshot.is_empty() {
                None
            } else {
                Some(req.from_snapshot.clone())
            },
            comms: comms.clone(),
            spec: crate::protocol::SandboxSpec {
                name: req.name.clone(),
                command: launch.command,
                entrypoint_len: launch.entrypoint_len,
                workdir: launch.workdir,
                user: launch.user,
                tty: req.tty,
                auto_remove: req.auto_remove,
            },
        };

        let mut info = self
            .backend
            .create_sandbox(id.clone(), &opts)
            .await
            .map_err(backend_err)?;
        // The backend has finished creating it, whatever it reports for a
        // sandbox it did not boot.
        info.status = crate::protocol::SandboxStatus::Running;

        // Capability token for this sandbox: a fresh random UUID, checked
        // by authorize() and never echoed back by get()/list(). Only its
        // hash is kept.
        let token = uuid::Uuid::new_v4().to_string();
        let meta = SandboxMeta {
            id: id.clone(),
            options: opts.clone(),
            created_at: info.created_at,
            status: info.status.clone(),
            exit_code: None,
            started_at: (!opts.spec.command.is_empty()).then_some(info.created_at),
            finished_at: None,
            token_hash: hash_token(&token),
        };
        info.started_at = meta.started_at;
        if let Err(e) = self.persist(meta.clone()).await {
            if let Err(remove_err) = self.backend.remove_sandbox(&id).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %remove_err,
                    "backend remove failed after a persist failure"
                );
            }
            return Err(e);
        }

        let egress = EgressProxy::new(id.clone(), egress_policy);
        let timeout_handle = (info.resources.timeout_seconds > 0).then(|| {
            self.spawn_timeout(
                &id,
                std::time::Duration::from_secs(info.resources.timeout_seconds),
            )
        });

        self.entries.write().await.insert(
            id.clone(),
            SandboxEntry {
                egress,
                timeout_handle,
                _creation_permit: permit,
                meta,
            },
        );

        // The entry now carries the name, so the in-flight claim can go.
        drop(name_reservation);

        // Register the comms policy with the broker so publish/subscribe
        // calls from this sandbox have something to match against. Done
        // after the entries-insert so we don't leak broker state if the
        // local registration fails — though entries.insert can't actually
        // fail here, the order keeps cleanup symmetric with remove().
        self.broker.register_sandbox(id.clone(), comms).await;

        metrics::gauge!("wardd_sandbox_active").increment(1.0);

        // Before the main process starts, so its `die` cannot come first.
        self.emit_event(&id, EventAction::Create, None).await;
        self.emit_event(&id, EventAction::Start, None).await;

        if let Err(e) = self.spawn_main(&id).await {
            self.fail_main_start(&id).await;
            if let Err(remove_err) = self.remove(&id).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %remove_err,
                    "remove failed after the main process did not start"
                );
            }
            return Err(e);
        }

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

        // The creator supplied the arguments, so echoing them back is safe.
        Ok(protocol_info_to_pb(info, CommandView::Full))
    }

    /// Claim `name` for a new sandbox. An empty name means no name and
    /// claims nothing. Fails when the name is invalid or already used by a
    /// live sandbox or an in-flight create.
    async fn reserve_name(&self, name: &str) -> Result<Option<NameReservation>> {
        if name.is_empty() {
            return Ok(None);
        }
        crate::validate::sandbox_name(name)?;
        let reservation = NameReservation::claim(&self.pending_names, name)
            .ok_or_else(|| name_in_use(name, None))?;
        // A create that finished before this claim has released its own, so
        // its name is in `entries` by now.
        let holder = self
            .entries
            .read()
            .await
            .iter()
            .find(|(_, entry)| entry.meta.options.spec.name == name)
            .map(|(id, _)| id.clone());
        if let Some(id) = holder {
            return Err(name_in_use(name, Some(&id)));
        }
        Ok(Some(reservation))
    }

    /// Resolve a sandbox name, id or id prefix to the full id, trying an
    /// exact name, then an exact id, then a unique id prefix. Names never
    /// look like ids (see `validate::sandbox_name`), so the order cannot
    /// pick the wrong sandbox.
    pub async fn resolve(&self, reference: &str) -> Result<String> {
        crate::validate::sandbox_reference(reference)?;
        let entries = self.entries.read().await;
        if let Some((id, _)) = entries
            .iter()
            .find(|(_, entry)| entry.meta.options.spec.name == reference)
        {
            return Ok(id.clone());
        }
        if entries.contains_key(reference) {
            return Ok(reference.to_string());
        }
        let mut matches: Vec<&str> = entries
            .keys()
            .filter(|id| id.starts_with(reference))
            .map(String::as_str)
            .collect();
        matches.sort_unstable();
        match matches.as_slice() {
            [] => Err(ApiError::SandboxNotFound(reference.to_string())),
            [id] => Ok((*id).to_string()),
            _ => Err(ApiError::AmbiguousReference(format!(
                "ID prefix \"{reference}\" matches several sandboxes: {}",
                matches.join(", ")
            ))),
        }
    }

    /// Remove sandbox `id` after `after` of wall time. The watcher routes
    /// the cleanup through `cleanup_sandbox_inner` (with from_timeout=true)
    /// so the bookkeeping side effects, entry removal, process record
    /// purge, broker deregistration, and the active-gauge decrement,
    /// match the user-initiated remove path exactly.
    fn spawn_timeout(&self, id: &str, after: std::time::Duration) -> tokio::task::JoinHandle<()> {
        let teardown = self.teardown();
        let sandbox_id = id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            tracing::info!(sandbox_id = %sandbox_id, "sandbox timeout reached, removing");
            if let Err(e) =
                Self::cleanup_sandbox_inner(&teardown, &sandbox_id, /* from_timeout = */ true).await
            {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "backend remove failed during timeout cleanup; local state still purged"
                );
            }
        })
    }

    /// Retrieve info for an existing sandbox. The command carries argv[0]
    /// only, because full arguments can hold secrets.
    pub async fn get(&self, id: &str) -> Result<PbSandboxInfo> {
        let id = self.resolve(id).await?;
        self.info(&id, CommandView::Argv0Only).await
    }

    /// Like [`get`](Self::get), but also returns the full command when the
    /// presented token matches the sandbox's token.
    pub async fn get_authorized(&self, id: &str, token: &str) -> Result<PbSandboxInfo> {
        let id = self.resolve(id).await?;
        self.authorize(&id, token).await?;
        self.info(&id, CommandView::Full).await
    }

    /// Backend info for `id`, with the state the manager recorded.
    async fn info(&self, id: &str, view: CommandView) -> Result<PbSandboxInfo> {
        let info = self.backend.get_sandbox(id).await.map_err(backend_err)?;
        let info = recorded_state(&*self.entries.read().await, redact_token(info));
        Ok(protocol_info_to_pb(info, view))
    }

    /// List all sandboxes.
    pub async fn list(&self) -> Result<Vec<PbSandboxInfo>> {
        let infos = self.backend.list_sandboxes().await.map_err(backend_err)?;
        let entries = self.entries.read().await;
        Ok(infos
            .into_iter()
            .map(redact_token)
            .map(|info| recorded_state(&entries, info))
            .map(|info| protocol_info_to_pb(info, CommandView::Argv0Only))
            .collect())
    }

    /// Verify a presented capability token matches the one minted for
    /// `sandbox_id` at creation. Distinguishes an unknown sandbox from a
    /// wrong token so callers (and the gRPC layer's status mapping) can
    /// tell the two apart.
    pub async fn authorize(&self, sandbox_id: &str, token: &str) -> Result<()> {
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
        let entries = self.entries.read().await;
        let entry = entries
            .get(sandbox_id)
            .ok_or_else(|| ApiError::SandboxNotFound(sandbox_id.to_string()))?;
        if token_hash_eq(&entry.meta.token_hash, &hash_token(token)) {
            Ok(())
        } else {
            Err(ApiError::PermissionDenied(format!(
                "token does not match sandbox {sandbox_id}"
            )))
        }
    }

    /// Return the egress decision log for a sandbox's proxy.
    pub async fn egress_log(&self, id: &str) -> Result<Vec<crate::egress::LogEntry>> {
        let id = self.resolve(id).await?;
        let id = id.as_str();
        let entries = self.entries.read().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        Ok(entry.egress.log_entries().await)
    }

    /// Remove a sandbox.
    pub async fn remove(&self, id: &str) -> Result<()> {
        let id = self.resolve(id).await?;
        let id = id.as_str();
        Self::cleanup_sandbox_inner(&self.teardown(), id, /* from_timeout = */ false)
            .await
            .map_err(backend_err)
    }

    // -----------------------------------------------------------------------
    // Stop, start, restart
    // -----------------------------------------------------------------------

    /// Stop a sandbox: the VM goes away and the rootfs stays. Stopping a
    /// stopped sandbox succeeds without doing anything.
    pub async fn stop(&self, reference: &str) -> Result<PbSandboxInfo> {
        self.stop_within(reference, None).await
    }

    /// Like [`stop`](Self::stop), with the seconds the main process gets
    /// after SIGTERM: `None` keeps the default, 0 forces it at once and a
    /// negative value waits as long as it takes.
    pub async fn stop_within(
        &self,
        reference: &str,
        grace_seconds: Option<i32>,
    ) -> Result<PbSandboxInfo> {
        let id = self.resolve(reference).await?;
        self.stop_resolved(&id, self.grace_period(grace_seconds))
            .await?;
        let info = self.info(&id, CommandView::Full).await?;
        if self.is_auto_remove(&id).await {
            self.remove(&id).await?;
        }
        Ok(info)
    }

    async fn is_auto_remove(&self, id: &str) -> bool {
        self.entries
            .read()
            .await
            .get(id)
            .is_some_and(|entry| entry.meta.options.spec.auto_remove)
    }

    /// Boot a stopped, exited or failed sandbox again. Starting a running
    /// sandbox succeeds without doing anything.
    pub async fn start(&self, reference: &str) -> Result<PbSandboxInfo> {
        let id = self.resolve(reference).await?;
        self.start_resolved(&id).await?;
        self.info(&id, CommandView::Full).await
    }

    /// Stop, then start. A failed sandbox has no VM to stop, so it only
    /// starts.
    pub async fn restart(&self, reference: &str) -> Result<PbSandboxInfo> {
        self.restart_within(reference, None).await
    }

    /// Like [`restart`](Self::restart), with the stop grace of
    /// [`stop_within`](Self::stop_within).
    pub async fn restart_within(
        &self,
        reference: &str,
        grace_seconds: Option<i32>,
    ) -> Result<PbSandboxInfo> {
        let id = self.resolve(reference).await?;
        let failed = self.recorded_status(&id).await? == crate::protocol::SandboxStatus::Failed;
        if !failed {
            self.stop_resolved(&id, self.grace_period(grace_seconds))
                .await?;
        }
        self.start_resolved(&id).await?;
        self.info(&id, CommandView::Full).await
    }

    async fn recorded_status(&self, id: &str) -> Result<crate::protocol::SandboxStatus> {
        self.entries
            .read()
            .await
            .get(id)
            .map(|entry| entry.meta.status.clone())
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))
    }

    /// The wait after SIGTERM a request asks for. `None` means no limit.
    fn grace_period(&self, grace_seconds: Option<i32>) -> Option<std::time::Duration> {
        match grace_seconds {
            None => Some(self.stop_grace),
            Some(seconds) => u64::try_from(seconds)
                .ok()
                .map(std::time::Duration::from_secs),
        }
    }

    async fn stop_resolved(&self, id: &str, grace: Option<std::time::Duration>) -> Result<()> {
        let Some((before, meta)) = self.begin_stop(id).await? else {
            tracing::info!(sandbox_id = %id, "sandbox already stopped");
            return Ok(());
        };
        tracing::info!(sandbox_id = %id, "stopping sandbox");
        let main_code = self.end_main_process(id, grace).await;
        let died_with = main_code.unwrap_or(0);
        if let Err(e) = self.backend.stop_sandbox(id).await {
            self.mark_failed(id).await;
            self.emit_die_if_was_running(&meta, &before, died_with)
                .await;
            return Err(backend_err(e));
        }
        self.processes
            .write()
            .await
            .retain(|_, rec| rec.sandbox_id != id);
        self.broker.deregister_sandbox(id).await;
        match main_code {
            Some(code) => {
                self.record_state(id, crate::protocol::SandboxStatus::Stopped, Some(code))
                    .await?;
            }
            None => {
                self.set_state(id, crate::protocol::SandboxStatus::Stopped)
                    .await?
            }
        }
        tracing::info!(sandbox_id = %id, "sandbox stopped");
        self.emit_die_if_was_running(&meta, &before, died_with)
            .await;
        self.emit_for(&meta, EventAction::Stop, None).await;
        Ok(())
    }

    /// Ask the main process to end with SIGTERM and force it with SIGKILL
    /// once the grace period passes. Returns its exit code, or `None` when
    /// the sandbox has no main process.
    async fn end_main_process(&self, id: &str, grace: Option<std::time::Duration>) -> Option<i32> {
        let (control, mut exit) = {
            let mains = self.mains.read().await;
            let main = mains.get(id)?;
            (main.control_tx.clone(), main.exit.clone())
        };
        if let Some(control) = &control {
            let _ = control.send(ProcessControl::Signal(SIGTERM)).await;
        }
        if !ended_within(&mut exit, grace).await {
            tracing::warn!(sandbox_id = %id, "main process ignored SIGTERM, forcing it");
            if let Some(control) = &control {
                let _ = control.send(ProcessControl::Signal(SIGKILL)).await;
            }
            ended_within(&mut exit, Some(self.stop_grace)).await;
        }
        let raw = (*exit.borrow()).map(decode_wait_status);
        Some(raw.unwrap_or(SIGNAL_EXIT_BASE + SIGKILL))
    }

    async fn start_resolved(&self, id: &str) -> Result<()> {
        let Some(comms) = self.begin_start(id).await? else {
            tracing::info!(sandbox_id = %id, "sandbox already running");
            return Ok(());
        };
        tracing::info!(sandbox_id = %id, "starting sandbox");
        if let Err(e) = self.backend.start_sandbox(id).await {
            self.mark_failed(id).await;
            return Err(backend_err(e));
        }
        self.commit_running_and_register(id, comms).await?;
        self.emit_event(id, EventAction::Start, None).await;
        if let Err(e) = self.spawn_main(id).await {
            self.fail_main_start(id).await;
            return Err(e);
        }
        tracing::info!(sandbox_id = %id, "sandbox started");
        Ok(())
    }

    /// Run the stored command as the sandbox's main process, if it has one.
    /// Output goes to the sandbox's buffer, which survives restarts so a
    /// later run appends to the earlier one.
    async fn spawn_main(&self, id: &str) -> Result<()> {
        let spec = {
            let entries = self.entries.read().await;
            let entry = entries
                .get(id)
                .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
            entry.meta.options.spec.clone()
        };
        if spec.command.is_empty() {
            return Ok(());
        }
        let mode = ExecMode {
            tty: spec.tty,
            raw: true,
        };
        let workdir = (!spec.workdir.is_empty()).then_some(spec.workdir);
        let handle = self
            .backend
            .exec(id, spec.command, workdir, HashMap::new(), mode)
            .await
            .map_err(backend_err)?;

        // Holding the entries lock keeps a concurrent remove from clearing
        // `mains` before this record lands.
        let entries = self.entries.read().await;
        if !entries.contains_key(id) {
            return Err(ApiError::SandboxNotFound(id.to_string()));
        }
        let mut mains = self.mains.write().await;
        let output = mains
            .get(id)
            .map(|m| Arc::clone(&m.output))
            .unwrap_or_else(|| Arc::new(OutputBuffer::new(self.output_cap)));
        let (exit_tx, exit) = watch::channel(None);
        if let Some(events) = handle.output_rx {
            tokio::spawn(pump_main_output(events, Arc::clone(&output), exit_tx));
        }
        tracing::info!(sandbox_id = %id, pid = %handle.pid, "main process started");
        tokio::spawn(mark_exited_on_main_end(
            self.teardown(),
            self.store.clone(),
            id.to_string(),
            handle.pid.clone(),
            exit.clone(),
            spec.auto_remove,
        ));
        mains.insert(
            id.to_string(),
            MainProcess {
                pid: handle.pid,
                output,
                stdin_tx: handle.stdin_tx,
                control_tx: handle.control_tx,
                exit,
            },
        );
        Ok(())
    }

    /// Id of the sandbox's current main process.
    pub async fn main_pid(&self, id: &str) -> Result<String> {
        self.mains
            .read()
            .await
            .get(id)
            .map(|m| m.pid.clone())
            .ok_or_else(|| no_main_process(id))
    }

    /// Output buffered so far, plus a receiver for what follows.
    pub async fn subscribe_main_output(
        &self,
        id: &str,
    ) -> Result<(Vec<OutputChunk>, broadcast::Receiver<OutputChunk>)> {
        let mains = self.mains.read().await;
        let main = mains.get(id).ok_or_else(|| no_main_process(id))?;
        Ok(main.output.subscribe())
    }

    /// Stdin and control channels of the sandbox's main process.
    pub async fn main_io(&self, id: &str) -> Result<ProcessIo> {
        let mains = self.mains.read().await;
        let main = mains.get(id).ok_or_else(|| no_main_process(id))?;
        Ok(ProcessIo {
            stdin: main.stdin_tx.clone(),
            control: main.control_tx.clone(),
        })
    }

    /// Unpack the tar in `chunks` into the directory `path` of a running
    /// sandbox. The guest agent enforces the copy policy while it unpacks.
    pub async fn copy_to(
        &self,
        sandbox_id: &str,
        path: &str,
        chunks: mpsc::Receiver<bytes::Bytes>,
    ) -> Result<()> {
        let id = self.copy_target(sandbox_id, path).await?;
        let target = copy::GuestTarget::new(sandbox_id, path);
        self.backend
            .copy_to(&id, path, chunks)
            .await
            .map_err(|e| target.backend_failure(e))
    }

    /// Stream a tar of the file or directory `path` of a running sandbox.
    /// The guest is untrusted, so each entry is checked against the copy
    /// policy before its bytes are forwarded.
    pub async fn copy_from(
        &self,
        sandbox_id: &str,
        path: &str,
    ) -> Result<mpsc::Receiver<Result<bytes::Bytes>>> {
        let id = self.copy_target(sandbox_id, path).await?;
        let target = copy::GuestTarget::new(sandbox_id, path);
        let source = self
            .backend
            .copy_from(&id, path)
            .await
            .map_err(|e| target.backend_failure(e))?;
        let limits = ward_archive::Limits::default();
        Ok(copy::checked_relay(source, limits, target))
    }

    /// List the processes of a running sandbox. `ps_args` are the options
    /// `docker top` passes to `ps`; the guest agent checks them.
    pub async fn top(
        &self,
        sandbox_id: &str,
        ps_args: &[String],
    ) -> Result<crate::backend::ProcessTable> {
        let id = self.resolve(sandbox_id).await?;
        if self.recorded_status(&id).await? != crate::protocol::SandboxStatus::Running {
            return Err(not_running(&id));
        }
        self.backend.top(&id, ps_args).await.map_err(backend_err)
    }

    /// Read the resource use of a running sandbox. The limit is the
    /// configured memory, or the guest's total memory when none is set.
    pub async fn stats(&self, sandbox_id: &str) -> Result<SandboxStats> {
        let id = self.resolve(sandbox_id).await?;
        let memory_mb = {
            let entries = self.entries.read().await;
            let entry = entries
                .get(&id)
                .ok_or_else(|| ApiError::SandboxNotFound(id.clone()))?;
            if entry.meta.status != crate::protocol::SandboxStatus::Running {
                return Err(not_running(&id));
            }
            entry.meta.options.resources.memory_mb
        };
        // Stamped before the call so the time matches the counters read.
        let read_at = std::time::SystemTime::now();
        let guest = match self.backend.stats(&id).await {
            Ok(guest) => guest,
            Err(e) => return Err(self.stats_failure(&id, e).await),
        };
        let configured = u64::from(memory_mb).saturating_mul(BYTES_PER_MIB);
        Ok(SandboxStats {
            cpu_nanos: guest.cpu_nanos,
            online_cpus: guest.online_cpus,
            mem_used_bytes: guest
                .mem_total_bytes
                .saturating_sub(guest.mem_available_bytes),
            mem_limit_bytes: if configured > 0 {
                configured
            } else {
                guest.mem_total_bytes
            },
            pids: guest.pids,
            read_at,
        })
    }

    /// A sandbox that stopped while it was being read is not running, not a
    /// backend failure.
    async fn stats_failure(&self, id: &str, error: BackendError) -> ApiError {
        match self.recorded_status(id).await {
            Ok(status) if status != crate::protocol::SandboxStatus::Running => not_running(id),
            _ => backend_err(error),
        }
    }

    /// Block until the sandbox's main process has ended or the sandbox has
    /// stopped, and return the exit code. A sandbox without a main process
    /// reports 0 once it is stopped. A sandbox that ends and is removed
    /// before the next look still reports the code its process left, and one
    /// removed while running reports the SIGKILL code its `die` event carries.
    pub async fn wait(&self, reference: &str) -> Result<i32> {
        use crate::protocol::SandboxStatus as State;
        let id = self.resolve(reference).await?;
        let mut live: Option<watch::Receiver<Option<i32>>> = None;
        loop {
            let recorded = self
                .entries
                .read()
                .await
                .get(&id)
                .map(|entry| (entry.meta.status.clone(), entry.meta.exit_code));
            let Some((status, exit_code)) = recorded else {
                let Some(exit) = live else {
                    return Err(ApiError::SandboxNotFound(id));
                };
                let raw = *exit.borrow();
                return Ok(raw.map_or(SIGNAL_EXIT_BASE + SIGKILL, decode_wait_status));
            };
            match status {
                State::Exited | State::Stopped => return Ok(exit_code.unwrap_or(0)),
                State::Failed => {
                    return Err(ApiError::InvalidRequest(format!(
                        "Container {id} failed to start"
                    )));
                }
                State::Running => {
                    // An earlier run's receiver already holds its status, so
                    // only one still waiting belongs to the current run.
                    if live.is_none() {
                        live = self.running_main_exit(&id).await;
                    }
                }
                State::Creating | State::Stopping => {}
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// The exit watch of the sandbox's main process, if it is still running.
    async fn running_main_exit(&self, id: &str) -> Option<watch::Receiver<Option<i32>>> {
        let mains = self.mains.read().await;
        let exit = mains.get(id)?.exit.clone();
        let running = exit.borrow().is_none();
        running.then_some(exit)
    }

    async fn copy_target(&self, sandbox_id: &str, path: &str) -> Result<String> {
        crate::validate::guest_path(path)?;
        let id = self.resolve(sandbox_id).await?;
        if self.recorded_status(&id).await? != crate::protocol::SandboxStatus::Running {
            return Err(not_running(&id));
        }
        Ok(id)
    }

    /// Send `signal` to the main process, or SIGKILL when `signal` is 0.
    pub async fn signal_main_process(&self, sandbox_id: &str, signal: i32) -> Result<()> {
        if !(0..=MAX_SIGNAL).contains(&signal) {
            return Err(ApiError::InvalidRequest(format!(
                "invalid signal: {signal}"
            )));
        }
        let id = self.resolve(sandbox_id).await?;
        if self.recorded_status(&id).await? != crate::protocol::SandboxStatus::Running {
            return Err(not_running(&id));
        }
        let (control, exit) = {
            let mains = self.mains.read().await;
            let main = mains.get(&id).ok_or_else(|| no_main_process(&id))?;
            let control = main
                .control_tx
                .clone()
                .ok_or_else(|| no_main_process(&id))?;
            (control, main.exit.clone())
        };
        if exit.borrow().is_some() {
            return Err(not_running(&id));
        }
        let signal = if signal == 0 { SIGKILL } else { signal };
        control
            .send(ProcessControl::Signal(signal))
            .await
            .map_err(|_| ApiError::ProcessNotFound(MAIN_PROCESS_PID.to_string()))
    }

    /// Drop the manager's hold on the main process's stdin, which sends EOF
    /// once no attached client holds a sender either.
    pub async fn close_main_stdin(&self, id: &str) {
        if let Some(main) = self.mains.write().await.get_mut(id) {
            main.stdin_tx = None;
        }
    }

    /// Start `command` in a running sandbox for an attached client, which owns
    /// the returned handle. It is not tracked as an exec process.
    pub async fn spawn_attached(
        &self,
        id: &str,
        command: Vec<String>,
        workdir: Option<String>,
        env: HashMap<String, String>,
        tty: bool,
    ) -> Result<ProcessHandle> {
        crate::validate::exec_command(&command)?;
        let mode = ExecMode { tty, raw: true };
        self.backend
            .exec(id, command, workdir, env, mode)
            .await
            .map_err(backend_err)
    }

    /// Wait for the main process to end and return its raw `wait(2)` status.
    pub async fn main_exit_status(&self, id: &str) -> Result<i32> {
        let mut exit = self
            .mains
            .read()
            .await
            .get(id)
            .map(|m| m.exit.clone())
            .ok_or_else(|| no_main_process(id))?;
        let status = exit.wait_for(Option::is_some).await.map_err(|_| {
            ApiError::Internal(format!(
                "main process of sandbox {id} ended without a status"
            ))
        })?;
        status.ok_or_else(|| {
            ApiError::Internal(format!("main process of sandbox {id} has no status"))
        })
    }

    /// Commit `running` and register the comms policy under one `entries`
    /// lock, so a concurrent cleanup cannot deregister in between and leave
    /// the broker holding a policy for a sandbox that is gone.
    async fn commit_running_and_register(
        &self,
        id: &str,
        comms: CommunicationPolicy,
    ) -> Result<()> {
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        let exit_code = entry.meta.exit_code;
        self.commit(entry, crate::protocol::SandboxStatus::Running, exit_code)
            .await?;
        self.broker.register_sandbox(id.to_string(), comms).await;
        Ok(())
    }

    /// Move a running or exited sandbox to `stopping` and return the state it
    /// left. `None` means it is already stopped and there is nothing to do.
    async fn begin_stop(
        &self,
        id: &str,
    ) -> Result<Option<(crate::protocol::SandboxStatus, SandboxMeta)>> {
        use crate::protocol::SandboxStatus as State;
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        let before = match entry.meta.status {
            State::Stopped => return Ok(None),
            State::Running | State::Exited => entry.meta.status.clone(),
            ref other => return Err(wrong_state(id, other, "stop")),
        };
        let exit_code = entry.meta.exit_code;
        let meta = entry.meta.clone();
        self.commit(entry, State::Stopping, exit_code).await?;
        Ok(Some((before, meta)))
    }

    /// Move a stopped, exited or failed sandbox to `creating` and clear its
    /// exit code. Returns the stored comms policy to register once it runs,
    /// or `None` when it is already running.
    async fn begin_start(&self, id: &str) -> Result<Option<CommunicationPolicy>> {
        use crate::protocol::SandboxStatus as State;
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        match entry.meta.status {
            State::Running => return Ok(None),
            State::Stopped | State::Exited | State::Failed => {}
            ref other => return Err(wrong_state(id, other, "start")),
        }
        self.commit(entry, State::Creating, None).await?;
        Ok(Some(entry.meta.options.comms.clone()))
    }

    /// Change the state and keep the exit code.
    async fn set_state(&self, id: &str, status: crate::protocol::SandboxStatus) -> Result<()> {
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        let exit_code = entry.meta.exit_code;
        self.commit(entry, status, exit_code).await
    }

    /// Record a failed transition. A sandbox removed meanwhile is already
    /// gone, so this only logs when it cannot save.
    async fn mark_failed(&self, id: &str) {
        if let Err(e) = self
            .set_state(id, crate::protocol::SandboxStatus::Failed)
            .await
        {
            tracing::warn!(sandbox_id = %id, error = %e, "cannot record the failed state");
        }
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
        teardown: &Teardown,
        id: &str,
        from_timeout: bool,
    ) -> std::result::Result<(), BackendError> {
        let removed = Self::detach_entry(teardown.entries.write().await.remove(id), from_timeout);
        Self::finish_cleanup(removed, teardown, id).await
    }

    /// Stop the removed entry's timer unless that timer is the caller.
    /// Returns the removed sandbox's record, if there was an entry.
    fn detach_entry(entry: Option<SandboxEntry>, from_timeout: bool) -> Option<SandboxMeta> {
        let entry = entry?;
        if !from_timeout && let Some(handle) = entry.timeout_handle {
            handle.abort();
        }
        Some(entry.meta)
    }

    /// Drop everything tied to a sandbox whose entry has left `entries`.
    async fn finish_cleanup(
        removed: Option<SandboxMeta>,
        teardown: &Teardown,
        id: &str,
    ) -> std::result::Result<(), BackendError> {
        teardown
            .processes
            .write()
            .await
            .retain(|_, rec| rec.sandbox_id != id);
        teardown.mains.write().await.remove(id);
        teardown.broker.deregister_sandbox(id).await;

        let result = teardown.backend.remove_sandbox(id).await;
        if let Some(meta) = removed {
            metrics::counter!("wardd_sandbox_remove_total").increment(1);
            metrics::gauge!("wardd_sandbox_active").decrement(1.0);
            // Removing a running sandbox kills it, as `docker rm -f` does.
            if meta.status == crate::protocol::SandboxStatus::Running {
                let code = meta.exit_code.unwrap_or(SIGNAL_EXIT_BASE + SIGKILL);
                teardown
                    .events
                    .emit(SandboxEvent::new(EventAction::Die, &meta).with_exit_code(code));
            }
            teardown
                .events
                .emit(SandboxEvent::new(EventAction::Destroy, &meta));
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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
        let sandbox_id = self.resolve(&req.sandbox_id).await?;
        crate::validate::exec_command(&req.command)?;
        let handle = self
            .backend
            .exec(
                &sandbox_id,
                req.command.clone(),
                if req.working_dir.is_empty() {
                    None
                } else {
                    Some(req.working_dir.clone())
                },
                req.env.clone(),
                crate::backend::ExecMode::default(),
            )
            .await
            .map_err(backend_err)?;

        // Park both channels under the pid: StreamOutput takes the receiver,
        // WriteStdin uses the sender. Either may be None if the backend
        // produced a process without that channel attached.
        let pid = handle.pid.clone();
        let record = ProcessRecord {
            sandbox_id: sandbox_id.clone(),
            output_rx: Arc::new(Mutex::new(handle.output_rx)),
            stdin_tx: handle.stdin_tx,
        };
        self.processes.write().await.insert(pid.clone(), record);

        Ok(ProcessInfo {
            pid,
            sandbox_id,
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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
        let sandbox_id = self.resolve(sandbox_id).await?;
        let sandbox_id = sandbox_id.as_str();
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

fn not_running(id: &str) -> ApiError {
    ApiError::InvalidRequest(format!("Container {id} is not running"))
}

fn no_main_process(id: &str) -> ApiError {
    ApiError::InvalidRequest(format!("sandbox {id} has no main process"))
}

/// The state the manager recorded wins over the backend's: only the
/// manager knows a boot failed.
fn recorded_state(
    entries: &HashMap<String, SandboxEntry>,
    mut info: crate::protocol::SandboxInfo,
) -> crate::protocol::SandboxInfo {
    if let Some(entry) = entries.get(&info.id) {
        info.status = entry.meta.status.clone();
        info.exit_code = entry.meta.exit_code;
        info.started_at = entry.meta.started_at;
        info.finished_at = entry.meta.finished_at;
    }
    info
}

fn wrong_state(id: &str, state: &crate::protocol::SandboxStatus, action: &str) -> ApiError {
    let state = format!("{state:?}").to_lowercase();
    ApiError::InvalidRequest(format!("sandbox {id} is {state}, cannot {action}"))
}

/// Clear the capability token before a sandbox's info is returned by
/// `get()`/`list()`. Only the `create()` response carries the real token;
/// echoing it back here would let anyone who can query the API bypass the
/// check `authorize()` performs.
fn redact_token(mut info: crate::protocol::SandboxInfo) -> crate::protocol::SandboxInfo {
    info.token = String::new();
    info
}

/// How much of a sandbox's command a response may carry.
#[derive(Clone, Copy)]
enum CommandView {
    Argv0Only,
    Full,
}

fn protocol_info_to_pb(info: crate::protocol::SandboxInfo, view: CommandView) -> PbSandboxInfo {
    use crate::protocol::SandboxStatus as ProtocolStatus;

    let status = match info.status {
        ProtocolStatus::Creating => SandboxStatus::Creating,
        ProtocolStatus::Running => SandboxStatus::Running,
        ProtocolStatus::Stopped => SandboxStatus::Stopped,
        ProtocolStatus::Failed => SandboxStatus::Failed,
        ProtocolStatus::Exited => SandboxStatus::Exited,
        ProtocolStatus::Stopping => SandboxStatus::Stopping,
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
        name: info.spec.name,
        exit_code: info.exit_code,
        started_at: info.started_at.map(system_time_to_timestamp),
        finished_at: info.finished_at.map(system_time_to_timestamp),
        workdir: info.spec.workdir,
        user: info.spec.user,
        command_name: info.spec.command.first().cloned().unwrap_or_default(),
        entrypoint: match view {
            CommandView::Full => info
                .spec
                .command
                .iter()
                .take(info.spec.entrypoint_len)
                .cloned()
                .collect(),
            CommandView::Argv0Only => Vec::new(),
        },
        command: match view {
            CommandView::Full => info.spec.command,
            CommandView::Argv0Only => Vec::new(),
        },
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
pub(crate) mod tests {
    use super::*;
    use crate::pb::{
        CommunicationMode as PbCommunicationMode, CommunicationPolicy as PbCommunicationPolicy,
        CreateSandboxRequest, EgressMode as PbEgressMode, EgressPolicy as PbEgressPolicy,
        ResourceLimits as PbResourceLimits,
    };
    use pretty_assertions::assert_eq;
    #[cfg(not(feature = "krunvm"))]
    use std::os::unix::ffi::OsStrExt;

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
        ) -> crate::backend::Result<crate::backend::image::PulledImage> {
            std::fs::create_dir_all(dest.join("bin")).map_err(crate::backend::BackendError::Io)?;
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(crate::backend::image::PulledImage::without_config(format!(
                "sha256:{hash:016x}"
            )))
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
        build_manager_with_puller(max_sandboxes, network_backend, Arc::new(FakePuller))
    }

    fn build_manager_with_puller(
        max_sandboxes: usize,
        network_backend: crate::config::NetworkBackendChoice,
        puller: Arc<dyn crate::backend::image::ImagePuller>,
    ) -> Arc<SandboxManager> {
        use crate::backend::image::ImageStore;
        use crate::backend::krunvm::KrunvmBackend;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, puller));
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
        assert_eq!(info.status, crate::pb::SandboxStatus::Running as i32);
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
        // response, to confirm the sandbox holds a non-empty token hash
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
            .meta
            .token_hash
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
        // manager's own entry map, that each holds a non-empty
        // token hash internally.
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
            assert!(
                !entries
                    .get(&a.id)
                    .expect("entry a")
                    .meta
                    .token_hash
                    .is_empty()
            );
            assert!(
                !entries
                    .get(&b.id)
                    .expect("entry b")
                    .meta
                    .token_hash
                    .is_empty()
            );
        }

        // Act
        let listed = mgr.list().await.expect("list");

        // Assert: list() never echoes any stored token back.
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|info| info.token.is_empty()));
    }

    // ----- image config resolution ----------------------------------------

    /// Records every reference it is asked to pull.
    #[derive(Debug, Default)]
    struct RecordingPuller {
        pulled: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingPuller {
        fn pulled(&self) -> Vec<String> {
            self.pulled.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::backend::image::ImagePuller for RecordingPuller {
        async fn pull(
            &self,
            reference: &str,
            dest: &std::path::Path,
        ) -> crate::backend::Result<crate::backend::image::PulledImage> {
            self.pulled.lock().unwrap().push(reference.to_string());
            FakePuller.pull(reference, dest).await
        }
    }

    fn build_recording_manager(
        max_sandboxes: usize,
    ) -> (Arc<SandboxManager>, Arc<RecordingPuller>) {
        let puller = Arc::new(RecordingPuller::default());
        let mgr = build_manager_with_puller(
            max_sandboxes,
            crate::config::NetworkBackendChoice::Smoltcp,
            Arc::clone(&puller) as Arc<dyn crate::backend::image::ImagePuller>,
        );
        (mgr, puller)
    }

    #[tokio::test]
    async fn given_snapshot_clone_when_create_with_image_config_then_image_is_not_pulled() {
        // Arrange
        let (mgr, puller) = build_recording_manager(4);
        let source = mgr.create(create_req("alpine"), "").await.unwrap();
        let snap = mgr.create_snapshot(&source.id, "label").await.unwrap();
        let req = CreateSandboxRequest {
            from_snapshot: snap.snapshot_id,
            use_image_config: true,
            ..create_req("busybox")
        };

        // Act
        mgr.create(req, &source.token).await.expect("clone");

        // Assert
        assert_eq!(puller.pulled(), vec!["alpine".to_string()]);
    }

    #[tokio::test]
    async fn given_sandbox_cap_reached_when_create_with_image_config_then_image_is_not_pulled() {
        // Arrange
        let (mgr, puller) = build_recording_manager(1);
        mgr.create(create_req("alpine"), "").await.unwrap();
        let req = CreateSandboxRequest {
            use_image_config: true,
            ..create_req("busybox")
        };

        // Act
        let err = mgr.create(req, "").await.expect_err("over the cap");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
        assert_eq!(puller.pulled(), vec!["alpine".to_string()]);
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
    async fn given_from_snapshot_with_other_sandbox_token_when_create_then_permission_denied() {
        // Arrange: two real sandboxes, each with its own token, and a
        // snapshot owned by A.
        let mgr = build_manager(4);
        let a = mgr.create(create_req("alpine:a"), "").await.unwrap();
        let b = mgr.create(create_req("alpine:b"), "").await.unwrap();
        let snap = mgr
            .create_snapshot(&a.id, "label")
            .await
            .expect("create_snapshot");
        let req = CreateSandboxRequest {
            from_snapshot: snap.snapshot_id,
            ..create_req("alpine")
        };

        // Act: B's valid token is presented for A's snapshot.
        let result = mgr.create(req, &b.token).await;

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

        // Act: characters outside the reference alphabet fail validation
        // before lookup.
        let err = mgr.get("not a valid id!").await.expect_err("malformed id");

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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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
                sandbox_id: "not a uuid!".into(),
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
    #[tokio::test]
    async fn given_sandbox_removed_when_stream_output_for_old_pid_then_sandbox_not_found() {
        // Arrange: removing a sandbox must drop its process records so
        // they do not accumulate. The sandbox reference no longer resolves,
        // so the lookup stops before the pid is read.
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
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
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
            .create_snapshot("not hex!", "x")
            .await
            .expect_err("malformed");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    // ----- persistence ---------------------------------------------------

    use crate::protocol::SandboxStatus as ProtoStatus;
    use crate::sandbox::store::{MetaStore, SandboxMeta, hash_token};

    /// A manager that persists under `path`. Calling it twice on one path
    /// models a daemon restart.
    fn manager_on(path: &std::path::Path, max_sandboxes: usize) -> Arc<SandboxManager> {
        manager_with_backend(path, max_sandboxes).0
    }

    fn manager_with_backend(
        path: &std::path::Path,
        max_sandboxes: usize,
    ) -> (
        Arc<SandboxManager>,
        Arc<crate::backend::krunvm::KrunvmBackend>,
    ) {
        use crate::backend::image::ImageStore;
        use crate::backend::krunvm::KrunvmBackend;
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        let krunvm = Arc::new(KrunvmBackend::with_image_store_for_test(
            path.to_path_buf(),
            store,
        ));
        let backend: Arc<dyn Backend> = krunvm.clone();
        let manager = Arc::new(
            SandboxManager::new(
                backend,
                Arc::new(Broker::new()),
                max_sandboxes,
                /* allow_host_mounts = */ false,
                crate::config::NetworkBackendChoice::Smoltcp,
            )
            .with_data_dir(path.to_path_buf()),
        );
        (manager, krunvm)
    }

    fn named_req() -> CreateSandboxRequest {
        CreateSandboxRequest {
            image: "alpine".into(),
            name: "web1".into(),
            command: vec!["sh".into(), "-c".into(), "echo hi".into()],
            workdir: "/srv".into(),
            tty: true,
            ..Default::default()
        }
    }

    async fn restart(path: &std::path::Path) -> Arc<SandboxManager> {
        let mgr = manager_on(path, 4);
        mgr.restore().await;
        mgr
    }

    #[tokio::test]
    async fn given_running_sandbox_when_daemon_restarts_then_listed_stopped_with_same_identity() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first.create(named_req(), "").await.unwrap();
        first
            .record_state(&created.id, ProtoStatus::Running, None)
            .await
            .unwrap();

        // Act
        let second = restart(tmp.path()).await;

        // Assert
        let listed = second.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].name, "web1");
        assert_eq!(listed[0].image, "alpine");
        assert_eq!(listed[0].status, crate::pb::SandboxStatus::Stopped as i32);
    }

    #[tokio::test]
    async fn given_command_tty_workdir_when_daemon_restarts_then_record_holds_them() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first.create(named_req(), "").await.unwrap();

        // Act
        let second = restart(tmp.path()).await;

        // Assert
        let on_disk = MetaStore::new(tmp.path()).load_all();
        let spec = &on_disk[0].options.spec;
        assert_eq!(spec.command, vec!["sh", "-c", "echo hi"]);
        assert!(spec.tty);
        assert_eq!(spec.workdir, "/srv");
        let token_view = second
            .get_authorized(&created.id, &created.token)
            .await
            .unwrap();
        assert_eq!(token_view.command, vec!["sh", "-c", "echo hi"]);
        assert_eq!(token_view.workdir, "/srv");
    }

    #[tokio::test]
    async fn given_exited_sandbox_with_code_7_when_daemon_restarts_then_still_exited_with_7() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first.create(named_req(), "").await.unwrap();
        first
            .record_state(&created.id, ProtoStatus::Exited, Some(7))
            .await
            .unwrap();

        // Act
        let second = restart(tmp.path()).await;

        // Assert
        let info = second.get(&created.id).await.unwrap();
        assert_eq!(info.status, crate::pb::SandboxStatus::Exited as i32);
        assert_eq!(info.exit_code, Some(7));
    }

    #[tokio::test]
    async fn given_right_token_when_authorize_after_restart_then_ok() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let created = manager_on(tmp.path(), 4)
            .create(named_req(), "")
            .await
            .unwrap();
        let second = restart(tmp.path()).await;

        // Act
        let result = second.authorize(&created.id, &created.token).await;

        // Assert
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn given_wrong_token_when_authorize_after_restart_then_permission_denied() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let created = manager_on(tmp.path(), 4)
            .create(named_req(), "")
            .await
            .unwrap();
        let second = restart(tmp.path()).await;

        // Act
        let result = second.authorize(&created.id, "not-the-token").await;

        // Assert
        assert!(matches!(result, Err(ApiError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn given_created_sandbox_when_meta_read_then_token_is_not_stored() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let created = manager_on(tmp.path(), 4)
            .create(named_req(), "")
            .await
            .unwrap();

        // Act
        let path = tmp
            .path()
            .join("sandboxes")
            .join(&created.id)
            .join("meta.json");
        let text = std::fs::read_to_string(path).unwrap();

        // Assert
        assert!(!text.contains(&created.token));
        assert!(text.contains(&hash_token(&created.token)));
    }

    #[rstest::rstest]
    #[case::stopped(ProtoStatus::Stopped)]
    #[case::exited(ProtoStatus::Exited)]
    #[tokio::test]
    async fn given_reloaded_sandbox_past_timeout_when_daemon_starts_then_removed(
        #[case] status: ProtoStatus,
    ) {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first
            .create(
                CreateSandboxRequest {
                    resources: Some(PbResourceLimits {
                        timeout_seconds: 60,
                        ..Default::default()
                    }),
                    ..named_req()
                },
                "",
            )
            .await
            .unwrap();
        let store = MetaStore::new(tmp.path());
        let mut meta: SandboxMeta = store.load_all().remove(0);
        meta.status = status;
        meta.created_at = std::time::SystemTime::now() - std::time::Duration::from_secs(61);
        store.save(&meta).unwrap();

        // Act
        let second = restart(tmp.path()).await;

        // Assert
        assert!(second.list().await.unwrap().is_empty());
        assert!(!tmp.path().join("sandboxes").join(&created.id).exists());
    }

    #[tokio::test]
    async fn given_reloaded_failed_sandbox_past_timeout_when_daemon_starts_then_removed() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first
            .create(
                CreateSandboxRequest {
                    resources: Some(PbResourceLimits {
                        timeout_seconds: 60,
                        ..Default::default()
                    }),
                    ..named_req()
                },
                "",
            )
            .await
            .unwrap();
        let store = MetaStore::new(tmp.path());
        let mut meta: SandboxMeta = store.load_all().remove(0);
        meta.status = ProtoStatus::Failed;
        meta.created_at = std::time::SystemTime::now() - std::time::Duration::from_secs(61);
        store.save(&meta).unwrap();

        // Act
        let second = restart(tmp.path()).await;

        // Assert
        assert!(second.list().await.unwrap().is_empty());
        assert!(!tmp.path().join("sandboxes").join(&created.id).exists());
    }

    #[tokio::test]
    async fn given_stale_agent_socket_at_real_path_when_daemon_restarts_then_removed() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let (first, backend) = manager_with_backend(tmp.path(), 4);
        let created = first.create(named_req(), "").await.unwrap();
        let socket = backend.agent_socket_path_for_test(&created.id);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"").unwrap();

        // Act
        let _second = restart(tmp.path()).await;

        // Assert
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn given_reloaded_sandbox_when_adopted_then_backend_rootfs_is_the_store_path() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let created = manager_on(tmp.path(), 4)
            .create(named_req(), "")
            .await
            .unwrap();
        let (second, backend) = manager_with_backend(tmp.path(), 4);

        // Act
        second.restore().await;

        // Assert
        let expected = MetaStore::new(tmp.path()).rootfs_path(&created.id);
        assert_eq!(backend.rootfs_for_test(&created.id).await, Some(expected));
    }

    #[tokio::test]
    async fn given_reloaded_sandboxes_when_cap_reached_then_create_fails() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 1);
        first.create(named_req(), "").await.unwrap();
        let second = manager_on(tmp.path(), 1);
        second.restore().await;

        // Act
        let result = second.create(create_req("alpine"), "").await;

        // Assert
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    // ----- names and id lookup -------------------------------------------

    const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const ID_A2: &str = "aaaaaaaa-0000-4000-8000-000000000002";
    const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000003";
    const SEED_TOKEN: &str = "11111111-2222-3333-4444-555555555555";
    const RACE_CREATORS: usize = 8;

    /// Write a stopped sandbox record with a fixed id so a restart loads it.
    fn seed_sandbox(path: &std::path::Path, id: &str, name: &str) {
        let spec = crate::protocol::SandboxSpec {
            name: name.to_string(),
            ..Default::default()
        };
        seed_record(path, id, spec, ProtoStatus::Stopped);
    }

    fn seed_record(
        path: &std::path::Path,
        id: &str,
        spec: crate::protocol::SandboxSpec,
        status: ProtoStatus,
    ) {
        let meta = SandboxMeta {
            id: id.to_string(),
            options: crate::protocol::CreateOpts {
                image: "alpine".into(),
                mounts: Vec::new(),
                volume_ids: Vec::new(),
                egress: Default::default(),
                resources: Default::default(),
                env: Default::default(),
                from_snapshot: None,
                comms: Default::default(),
                spec,
            },
            created_at: std::time::SystemTime::now(),
            status,
            exit_code: None,
            started_at: None,
            finished_at: None,
            token_hash: hash_token(SEED_TOKEN),
        };
        MetaStore::new(path).save(&meta).unwrap();
    }

    fn named(name: &str) -> CreateSandboxRequest {
        CreateSandboxRequest {
            image: "alpine".into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn given_two_sandboxes_sharing_a_prefix_when_prefix_resolved_then_ambiguous_naming_both_ids()
     {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        seed_sandbox(tmp.path(), ID_A, "");
        seed_sandbox(tmp.path(), ID_A2, "");
        let mgr = restart(tmp.path()).await;

        // Act
        let err = mgr.get("aaaa").await.expect_err("ambiguous prefix");

        // Assert
        let ApiError::AmbiguousReference(msg) = err else {
            panic!("expected AmbiguousReference, got {err:?}");
        };
        assert!(msg.contains(ID_A) && msg.contains(ID_A2), "got: {msg}");
    }

    #[rstest::rstest]
    #[case::exited(ProtoStatus::Exited)]
    #[case::stopped(ProtoStatus::Stopped)]
    #[tokio::test]
    async fn given_finished_auto_remove_record_when_restored_then_it_is_removed(
        #[case] status: ProtoStatus,
    ) {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let spec = crate::protocol::SandboxSpec {
            name: "gone".into(),
            auto_remove: true,
            ..Default::default()
        };
        seed_record(tmp.path(), ID_A, spec, status);

        // Act
        let mgr = restart(tmp.path()).await;

        // Assert
        assert!(matches!(
            mgr.get(ID_A).await,
            Err(ApiError::SandboxNotFound(_))
        ));
        assert!(MetaStore::new(tmp.path()).load_all().is_empty());
        assert_eq!(mgr.entries.read().await.len(), 0);
        assert_eq!(mgr.create_semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn given_full_uuid_when_get_then_returns_that_sandbox() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        seed_sandbox(tmp.path(), ID_A, "");
        let mgr = restart(tmp.path()).await;

        // Act
        let info = mgr.get(ID_A).await.unwrap();

        // Assert
        assert_eq!(info.id, ID_A);
    }

    #[tokio::test]
    async fn given_unique_prefix_when_get_then_resolves_to_that_sandbox() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        seed_sandbox(tmp.path(), ID_A, "");
        seed_sandbox(tmp.path(), ID_B, "");
        let mgr = restart(tmp.path()).await;

        // Act
        let info = mgr.get("bbbb").await.unwrap();

        // Assert
        assert_eq!(info.id, ID_B);
    }

    #[tokio::test]
    async fn given_unmatched_prefix_when_resolve_then_sandbox_not_found() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        seed_sandbox(tmp.path(), ID_A, "");
        let mgr = restart(tmp.path()).await;

        // Act
        let err = mgr.resolve("cccc").await.expect_err("no match");

        // Assert
        assert!(matches!(err, ApiError::SandboxNotFound(_)));
    }

    #[tokio::test]
    async fn given_empty_reference_when_resolve_then_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr.resolve("").await.expect_err("empty prefix");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[rstest::rstest]
    #[case::single_char("w")]
    #[case::max_length("g".repeat(crate::validate::MAX_SANDBOX_NAME_LEN))]
    #[case::hyphen("web-1")]
    #[case::underscore("web_1")]
    #[case::dot("web.1")]
    #[tokio::test]
    async fn given_valid_name_when_create_then_succeeds(#[case] name: String) {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let created = mgr.create(named(&name), "").await.unwrap();

        // Assert
        assert_eq!(created.name, name);
    }

    #[rstest::rstest]
    #[case::bad_char_at("web_1@host".to_string())]
    #[case::bad_char_bang("web-1!".to_string())]
    #[case::space("web 1".to_string())]
    #[case::over_length("g".repeat(crate::validate::MAX_SANDBOX_NAME_LEN + 1))]
    #[case::hex_only("a1".to_string())]
    #[case::uuid_prefix("550e8400-e29b".to_string())]
    #[tokio::test]
    async fn given_invalid_name_when_create_then_rejected(#[case] name: String) {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr.create(named(&name), "").await.expect_err("bad name");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
        assert!(mgr.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn given_no_name_when_two_sandboxes_created_then_both_succeed() {
        // Arrange
        let mgr = build_manager(4);
        mgr.create(named(""), "").await.unwrap();

        // Act
        let second = mgr.create(named(""), "").await;

        // Assert
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn given_named_sandbox_when_get_by_name_then_returns_it() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(named("web1"), "").await.unwrap();

        // Act
        let info = mgr.get("web1").await.unwrap();

        // Assert
        assert_eq!(info.id, created.id);
    }

    #[tokio::test]
    async fn given_named_sandbox_when_authorize_by_name_then_checks_token_of_resolved_id() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(named("web1"), "").await.unwrap();

        // Act
        let right = mgr.authorize("web1", &created.token).await;
        let wrong = mgr.authorize("web1", "wrong").await;

        // Assert
        assert!(right.is_ok());
        assert!(matches!(wrong, Err(ApiError::PermissionDenied(_))));
    }

    #[cfg(not(feature = "krunvm"))] // needs a real guest agent; covered by e2e_vm
    #[tokio::test]
    async fn given_named_sandbox_when_exec_by_name_then_process_reports_resolved_id() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(named("web1"), "").await.unwrap();

        // Act
        let process = mgr
            .exec(crate::pb::ExecRequest {
                sandbox_id: "web1".into(),
                command: vec!["echo".into()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Assert
        assert_eq!(process.sandbox_id, created.id);
    }

    #[tokio::test]
    async fn given_named_sandbox_when_remove_by_name_then_name_is_free_again() {
        // Arrange
        let mgr = build_manager(4);
        mgr.create(named("web1"), "").await.unwrap();

        // Act
        mgr.remove("web1").await.unwrap();
        let again = mgr.create(named("web1"), "").await;

        // Assert
        assert!(again.is_ok());
    }

    #[tokio::test]
    async fn given_sandbox_named_web1_when_another_created_with_that_name_then_rejected() {
        // Arrange
        let mgr = build_manager(4);
        let first = mgr.create(named("web1"), "").await.unwrap();

        // Act
        let err = mgr
            .create(named("web1"), "")
            .await
            .expect_err("duplicate name");

        // Assert
        assert!(
            matches!(&err, ApiError::NameInUse { name, id } if name == "web1" && id.as_deref() == Some(first.id.as_str()))
        );
        assert_eq!(mgr.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn given_reloaded_sandbox_named_web1_when_created_with_that_name_then_rejected() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        seed_sandbox(tmp.path(), ID_A, "web1");
        let mgr = restart(tmp.path()).await;

        // Act
        let result = mgr.create(named("web1"), "").await;

        // Assert
        assert!(matches!(result, Err(ApiError::NameInUse { .. })));
    }

    #[tokio::test]
    async fn given_concurrent_creates_of_one_name_when_finished_then_exactly_one_succeeds() {
        // Arrange
        let mgr = build_manager(RACE_CREATORS);
        let tasks: Vec<_> = (0..RACE_CREATORS)
            .map(|_| {
                let mgr = Arc::clone(&mgr);
                tokio::spawn(async move { mgr.create(named("web1"), "").await })
            })
            .collect();

        // Act
        let mut winners = 0;
        for task in tasks {
            if task.await.unwrap().is_ok() {
                winners += 1;
            }
        }

        // Assert
        assert_eq!(winners, 1);
        assert_eq!(mgr.list().await.unwrap().len(), 1);
    }

    // ----- stop, start, restart ------------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::pb::{CommunicationMode as PbMode, CommunicationPolicy as PbPolicy};

    const WAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const WAIT_STEP: std::time::Duration = std::time::Duration::from_millis(100);
    const SHORT_TIMEOUT_SECONDS: u64 = 1;
    const GROUP_NAME: &str = "workers";
    const RACE_ROUNDS: usize = 500;
    const RACE_SANDBOX_SLOTS: usize = 2;
    const KEPT_FILE: &str = "kept.txt";
    const KEPT_BYTES: &[u8] = b"still here\n";

    /// Delegates to the stub backend, but `start_sandbox` fails while
    /// `fail_start` is set, standing in for a VM that does not boot.
    struct BootGate {
        inner: Arc<crate::backend::krunvm::KrunvmBackend>,
        fail_start: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Backend for BootGate {
        async fn create_sandbox(
            &self,
            id: String,
            opts: &CreateOpts,
        ) -> crate::backend::Result<crate::protocol::SandboxInfo> {
            self.inner.create_sandbox(id, opts).await
        }
        async fn get_sandbox(
            &self,
            id: &str,
        ) -> crate::backend::Result<crate::protocol::SandboxInfo> {
            self.inner.get_sandbox(id).await
        }
        async fn list_sandboxes(
            &self,
        ) -> crate::backend::Result<Vec<crate::protocol::SandboxInfo>> {
            self.inner.list_sandboxes().await
        }
        async fn remove_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            self.inner.remove_sandbox(id).await
        }
        async fn adopt_sandbox(
            &self,
            info: crate::protocol::SandboxInfo,
            opts: CreateOpts,
            rootfs: std::path::PathBuf,
        ) -> crate::backend::Result<()> {
            self.inner.adopt_sandbox(info, opts, rootfs).await
        }
        async fn stop_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            self.inner.stop_sandbox(id).await
        }
        async fn start_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            if self.fail_start.load(Ordering::SeqCst) {
                return Err(BackendError::Spawn("boot failed".into()));
            }
            self.inner.start_sandbox(id).await
        }
        async fn count(&self) -> crate::backend::Result<usize> {
            self.inner.count().await
        }
        async fn exec(
            &self,
            sandbox_id: &str,
            command: Vec<String>,
            working_dir: Option<String>,
            env: HashMap<String, String>,
            mode: crate::backend::ExecMode,
        ) -> crate::backend::Result<crate::backend::ProcessHandle> {
            self.inner
                .exec(sandbox_id, command, working_dir, env, mode)
                .await
        }
        async fn kill_process(&self, sandbox_id: &str, pid: &str) -> crate::backend::Result<()> {
            self.inner.kill_process(sandbox_id, pid).await
        }
        async fn copy_to(
            &self,
            sandbox_id: &str,
            path: &str,
            chunks: tokio::sync::mpsc::Receiver<bytes::Bytes>,
        ) -> crate::backend::Result<()> {
            self.inner.copy_to(sandbox_id, path, chunks).await
        }
        async fn copy_from(
            &self,
            sandbox_id: &str,
            path: &str,
        ) -> crate::backend::Result<tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>>
        {
            self.inner.copy_from(sandbox_id, path).await
        }
        async fn top(
            &self,
            sandbox_id: &str,
            ps_args: &[String],
        ) -> crate::backend::Result<crate::backend::ProcessTable> {
            self.inner.top(sandbox_id, ps_args).await
        }
        async fn stats(
            &self,
            sandbox_id: &str,
        ) -> crate::backend::Result<crate::backend::GuestStats> {
            self.inner.stats(sandbox_id).await
        }
        async fn create_snapshot(
            &self,
            sandbox_id: &str,
            label: &str,
        ) -> crate::backend::Result<crate::protocol::SnapshotInfo> {
            self.inner.create_snapshot(sandbox_id, label).await
        }
        async fn restore_snapshot(
            &self,
            sandbox_id: &str,
            snapshot_id: &str,
        ) -> crate::backend::Result<()> {
            self.inner.restore_snapshot(sandbox_id, snapshot_id).await
        }
        async fn list_snapshots(
            &self,
            sandbox_id: &str,
        ) -> crate::backend::Result<Vec<crate::protocol::SnapshotInfo>> {
            self.inner.list_snapshots(sandbox_id).await
        }
        async fn snapshot_owner(&self, snapshot_id: &str) -> crate::backend::Result<String> {
            self.inner.snapshot_owner(snapshot_id).await
        }
    }

    /// A persisting manager whose backend boot can be made to fail.
    fn manager_with_boot_gate(
        path: &std::path::Path,
        max_sandboxes: usize,
    ) -> (Arc<SandboxManager>, Arc<AtomicBool>) {
        use crate::backend::image::ImageStore;
        use crate::backend::krunvm::KrunvmBackend;
        let store = Arc::new(ImageStore::with_puller(
            path.join("cache").join("images"),
            64,
            Arc::new(FakePuller),
        ));
        let fail_start = Arc::new(AtomicBool::new(false));
        let backend: Arc<dyn Backend> = Arc::new(BootGate {
            inner: Arc::new(KrunvmBackend::with_image_store_for_test(
                path.to_path_buf(),
                store,
            )),
            fail_start: Arc::clone(&fail_start),
        });
        let manager = SandboxManager::new(
            backend,
            Arc::new(Broker::new()),
            max_sandboxes,
            /* allow_host_mounts = */ false,
            crate::config::NetworkBackendChoice::Smoltcp,
        )
        .with_data_dir(path.to_path_buf());
        (Arc::new(manager), fail_start)
    }

    async fn status_of(mgr: &SandboxManager, id: &str) -> SandboxStatus {
        let info = mgr.get(id).await.unwrap();
        SandboxStatus::try_from(info.status).unwrap()
    }

    /// Poll `done` every `WAIT_STEP` until it holds or `WAIT_DEADLINE` passes.
    async fn wait_until<F, Fut>(mut done: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + WAIT_DEADLINE;
        while std::time::Instant::now() < deadline {
            if done().await {
                return true;
            }
            tokio::time::sleep(WAIT_STEP).await;
        }
        false
    }

    #[tokio::test]
    async fn given_running_sandbox_without_command_when_stopped_and_started_then_it_runs_again() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();

        // Act
        let stopped = mgr.stop(&created.id).await.unwrap();
        let after_stop = status_of(&mgr, &created.id).await;
        let started = mgr.start(&created.id).await.unwrap();

        // Assert
        assert_eq!(stopped.status, SandboxStatus::Stopped as i32);
        assert_eq!(after_stop, SandboxStatus::Stopped);
        assert_eq!(started.status, SandboxStatus::Running as i32);
        assert_eq!(status_of(&mgr, &created.id).await, SandboxStatus::Running);
    }

    #[tokio::test]
    async fn given_stopped_sandbox_when_stopped_again_then_it_succeeds_and_stays_stopped() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();

        // Act
        let again = mgr.stop(&created.id).await;

        // Assert
        assert_eq!(again.unwrap().status, SandboxStatus::Stopped as i32);
    }

    #[tokio::test]
    async fn given_running_sandbox_when_started_then_it_succeeds_and_stays_running() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();

        // Act
        let started = mgr.start(&created.id).await;

        // Assert
        assert_eq!(started.unwrap().status, SandboxStatus::Running as i32);
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_stop_start_or_restart_then_not_found() {
        // Arrange
        let mgr = build_manager(2);
        let id = "aaaaaaaa-0000-0000-0000-000000000000";

        // Act
        let results = [
            mgr.stop(id).await,
            mgr.start(id).await,
            mgr.restart(id).await,
        ];

        // Assert
        for result in results {
            assert!(matches!(result, Err(ApiError::SandboxNotFound(_))));
        }
    }

    #[tokio::test]
    async fn given_stopped_sandbox_when_restarted_then_it_is_running() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();

        // Act
        let restarted = mgr.restart(&created.id).await.unwrap();

        // Assert
        assert_eq!(restarted.status, SandboxStatus::Running as i32);
    }

    #[tokio::test]
    async fn given_running_sandbox_when_restarted_then_it_is_running() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();

        // Act
        let restarted = mgr.restart(&created.id).await.unwrap();

        // Assert
        assert_eq!(restarted.status, SandboxStatus::Running as i32);
    }

    #[tokio::test]
    async fn given_boot_failure_when_started_then_sandbox_is_failed_and_persisted_as_failed() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, fail_start) = manager_with_boot_gate(tmp.path(), 2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();
        fail_start.store(true, Ordering::SeqCst);

        // Act
        let result = mgr.start(&created.id).await;

        // Assert
        assert!(matches!(result, Err(ApiError::Backend(_))));
        assert_eq!(status_of(&mgr, &created.id).await, SandboxStatus::Failed);
        let reloaded = restart(tmp.path()).await;
        assert_eq!(
            status_of(&reloaded, &created.id).await,
            SandboxStatus::Failed
        );
    }

    #[tokio::test]
    async fn given_failed_sandbox_when_started_again_after_the_fault_clears_then_it_runs() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, fail_start) = manager_with_boot_gate(tmp.path(), 2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();
        fail_start.store(true, Ordering::SeqCst);
        mgr.start(&created.id).await.unwrap_err();
        fail_start.store(false, Ordering::SeqCst);

        // Act
        let started = mgr.start(&created.id).await.unwrap();

        // Assert
        assert_eq!(started.status, SandboxStatus::Running as i32);
    }

    #[tokio::test]
    async fn given_failed_sandbox_when_stopped_then_invalid_request() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, fail_start) = manager_with_boot_gate(tmp.path(), 2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();
        fail_start.store(true, Ordering::SeqCst);
        mgr.start(&created.id).await.unwrap_err();

        // Act
        let result = mgr.stop(&created.id).await;

        // Assert
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn given_failed_sandbox_when_restarted_after_the_fault_clears_then_it_runs() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, fail_start) = manager_with_boot_gate(tmp.path(), 2);
        let created = mgr.create(create_req("alpine"), "").await.unwrap();
        mgr.stop(&created.id).await.unwrap();
        fail_start.store(true, Ordering::SeqCst);
        mgr.start(&created.id).await.unwrap_err();
        fail_start.store(false, Ordering::SeqCst);

        // Act
        let restarted = mgr.restart(&created.id).await.unwrap();

        // Assert
        assert_eq!(restarted.status, SandboxStatus::Running as i32);
    }

    #[tokio::test]
    async fn given_stopped_sandbox_when_daemon_restarts_then_it_is_still_stopped_and_can_start() {
        // Arrange
        let tmp = tempfile::tempdir().unwrap();
        let first = manager_on(tmp.path(), 4);
        let created = first.create(named_req(), "").await.unwrap();
        first.stop(&created.id).await.unwrap();
        let file = MetaStore::new(tmp.path())
            .rootfs_path(&created.id)
            .join(KEPT_FILE);
        std::fs::write(&file, KEPT_BYTES).unwrap();

        // Act
        let second = restart(tmp.path()).await;
        second.authorize(&created.id, &created.token).await.unwrap();
        let started = second.start(&created.id).await.unwrap();

        // Assert
        assert_eq!(started.status, SandboxStatus::Running as i32);
        assert_eq!(std::fs::read(&file).unwrap(), KEPT_BYTES);
    }

    #[tokio::test]
    async fn given_max_sandboxes_with_one_stopped_when_another_is_created_then_limit_error() {
        // Arrange
        let mgr = build_manager(2);
        let first = mgr.create(create_req("alpine:1"), "").await.unwrap();
        mgr.create(create_req("alpine:2"), "").await.unwrap();
        mgr.stop(&first.id).await.unwrap();

        // Act
        let result = mgr.create(create_req("alpine:3"), "").await;

        // Assert
        match result {
            Err(ApiError::InvalidRequest(msg)) => assert!(msg.contains("limit"), "{msg}"),
            other => panic!("expected the sandbox limit error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn given_stopped_sandbox_with_timeout_when_it_expires_then_it_is_removed() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr
            .create(
                CreateSandboxRequest {
                    resources: Some(PbResourceLimits {
                        timeout_seconds: SHORT_TIMEOUT_SECONDS,
                        ..Default::default()
                    }),
                    ..create_req("alpine")
                },
                "",
            )
            .await
            .unwrap();
        mgr.stop(&created.id).await.unwrap();

        // Act
        let removed = wait_until(|| async { mgr.list().await.unwrap().is_empty() }).await;

        // Assert
        assert!(removed, "the stopped sandbox outlived its timeout");
    }

    #[tokio::test]
    async fn given_group_policy_when_stopped_and_started_then_broker_forgets_then_restores_it() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr
            .create(
                CreateSandboxRequest {
                    comms: Some(PbPolicy {
                        mode: PbMode::Group as i32,
                        group: GROUP_NAME.into(),
                    }),
                    ..create_req("alpine")
                },
                "",
            )
            .await
            .unwrap();

        // Act
        mgr.stop(&created.id).await.unwrap();
        let while_stopped = mgr.broker().subscribe(&created.id, "topic").await;
        mgr.start(&created.id).await.unwrap();
        let after_start = mgr.broker().subscribe(&created.id, "topic").await;

        // Assert
        assert!(matches!(while_stopped, Err(ApiError::SandboxNotFound(_))));
        assert!(after_start.is_ok(), "start must restore the stored policy");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn given_start_racing_remove_when_both_finish_then_broker_holds_no_policy_for_removed_sandbox()
     {
        // Arrange
        let mgr = build_manager(RACE_SANDBOX_SLOTS);
        for _ in 0..RACE_ROUNDS {
            let created = mgr.create(create_req("alpine"), "").await.unwrap();
            mgr.stop(&created.id).await.unwrap();

            // Act
            let starter = {
                let mgr = Arc::clone(&mgr);
                let id = created.id.clone();
                tokio::spawn(async move { mgr.start(&id).await })
            };
            let remover = {
                let mgr = Arc::clone(&mgr);
                let id = created.id.clone();
                tokio::spawn(async move {
                    while status_of(&mgr, &id).await != SandboxStatus::Running {
                        tokio::task::yield_now().await;
                    }
                    mgr.remove(&id).await
                })
            };
            let _ = starter.await.unwrap();
            let _ = remover.await.unwrap();

            // Assert
            let gone = matches!(
                mgr.get(&created.id).await,
                Err(ApiError::SandboxNotFound(_))
            );
            let subscribed = mgr.broker().subscribe(&created.id, "topic").await;
            assert!(
                !gone || matches!(subscribed, Err(ApiError::SandboxNotFound(_))),
                "broker kept a policy for removed sandbox {}",
                created.id
            );
        }
    }

    #[tokio::test]
    async fn given_named_sandbox_when_stopped_by_name_then_that_sandbox_stops() {
        // Arrange
        let mgr = build_manager(2);
        let created = mgr.create(named("web1"), "").await.unwrap();

        // Act
        let stopped = mgr.stop("web1").await.unwrap();

        // Assert
        assert_eq!(stopped.id, created.id);
        assert_eq!(stopped.status, SandboxStatus::Stopped as i32);
    }

    // ----- main process --------------------------------------------------

    /// Backend that keeps the stub's lifecycle but runs `exec` as a real
    /// host process (on a pty when asked), so the main process tests see
    /// real output, pids and wait statuses.
    #[cfg(not(feature = "krunvm"))]
    struct HostBackend {
        inner: crate::backend::krunvm::KrunvmBackend,
        fail_stop: bool,
    }

    #[cfg(not(feature = "krunvm"))]
    fn stream_event(kind: StreamEventKind, code: Option<i32>, status: Option<i32>) -> StreamEvent {
        StreamEvent {
            kind,
            line: String::new(),
            exit_code: code,
            wait_status: status,
            timestamp: std::time::SystemTime::now(),
            duration_ms: 0,
        }
    }

    #[cfg(not(feature = "krunvm"))]
    async fn forward_output(
        mut source: impl tokio::io::AsyncRead + Unpin,
        stream: RawStream,
        tx: mpsc::Sender<StreamEvent>,
    ) {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1024];
        // A pty master reports EIO once the child is gone, which ends it too.
        while let Ok(n @ 1..) = source.read(&mut buf).await {
            let kind = StreamEventKind::Data {
                stream,
                bytes: buf[..n].to_vec(),
            };
            if tx.send(stream_event(kind, None, None)).await.is_err() {
                break;
            }
        }
    }

    #[cfg(not(feature = "krunvm"))]
    fn open_pty() -> (std::fs::File, std::fs::File) {
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let name = ptsname(&master, Vec::new()).expect("ptsname");
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::ffi::OsStr::from_bytes(name.to_bytes()))
            .expect("open slave");
        (std::fs::File::from(master), slave)
    }

    #[cfg(not(feature = "krunvm"))]
    #[async_trait::async_trait]
    impl Backend for HostBackend {
        async fn exec(
            &self,
            sandbox_id: &str,
            command: Vec<String>,
            _working_dir: Option<String>,
            _env: HashMap<String, String>,
            mode: ExecMode,
        ) -> crate::backend::Result<crate::backend::ProcessHandle> {
            use std::process::Stdio;
            let mut cmd = tokio::process::Command::new(&command[0]);
            cmd.args(&command[1..]).kill_on_drop(true);
            let (output_tx, output_rx) = mpsc::channel(16);
            let (stdin_tx, mut stdin_rx) = mpsc::channel::<bytes::Bytes>(16);
            let mut readers = Vec::new();
            let mut child_stdin = None;
            let mut pty_master = None;
            let mut child = if mode.tty {
                let (master, slave) = open_pty();
                pty_master = Some(master.try_clone()?);
                cmd.stdin(slave.try_clone()?)
                    .stdout(slave.try_clone()?)
                    .stderr(slave);
                let child = cmd.spawn()?;
                readers.push(tokio::spawn(forward_output(
                    tokio::fs::File::from_std(master),
                    RawStream::Stdout,
                    output_tx.clone(),
                )));
                child
            } else {
                cmd.stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = cmd.spawn()?;
                child_stdin = child.stdin.take();
                readers.push(tokio::spawn(forward_output(
                    child.stdout.take().expect("stdout"),
                    RawStream::Stdout,
                    output_tx.clone(),
                )));
                readers.push(tokio::spawn(forward_output(
                    child.stderr.take().expect("stderr"),
                    RawStream::Stderr,
                    output_tx.clone(),
                )));
                child
            };
            drop(cmd);
            let pid = child.id().expect("pid").to_string();
            let (control_tx, mut control_rx) = mpsc::channel::<ProcessControl>(16);
            let os_pid = rustix::process::Pid::from_raw(pid.parse().expect("numeric pid"));
            tokio::spawn(async move {
                while let Some(control) = control_rx.recv().await {
                    match (control, os_pid, &pty_master) {
                        (ProcessControl::Signal(signo), Some(pid), _) => {
                            let signal = rustix::process::Signal::from_named_raw(signo);
                            if let Some(signal) = signal {
                                let _ = rustix::process::kill_process(pid, signal);
                            }
                        }
                        (ProcessControl::Resize { rows, cols }, _, Some(master)) => {
                            let size = rustix::termios::Winsize {
                                ws_row: rows as u16,
                                ws_col: cols as u16,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            let _ = rustix::termios::tcsetwinsize(master, size);
                        }
                        _ => {}
                    }
                }
            });
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(chunk) = stdin_rx.recv().await {
                    match child_stdin.as_mut() {
                        Some(stdin) if chunk.is_empty() => drop(stdin.shutdown().await),
                        Some(stdin) => drop(stdin.write_all(&chunk).await),
                        None => {}
                    }
                }
                // Every sender dropped: the stdin pipe closes.
            });
            tokio::spawn(async move {
                use std::os::unix::process::ExitStatusExt;
                let status = child.wait().await.expect("wait");
                for reader in readers {
                    let _ = reader.await;
                }
                let event = stream_event(
                    StreamEventKind::Exit,
                    status.code(),
                    Some(status.into_raw()),
                );
                let _ = output_tx.send(event).await;
            });
            Ok(crate::backend::ProcessHandle {
                pid,
                sandbox_id: sandbox_id.to_string(),
                stdin_tx: Some(stdin_tx),
                output_rx: Some(output_rx),
                control_tx: Some(control_tx),
            })
        }

        async fn create_sandbox(
            &self,
            id: String,
            opts: &CreateOpts,
        ) -> crate::backend::Result<crate::protocol::SandboxInfo> {
            self.inner.create_sandbox(id, opts).await
        }
        async fn get_sandbox(
            &self,
            id: &str,
        ) -> crate::backend::Result<crate::protocol::SandboxInfo> {
            self.inner.get_sandbox(id).await
        }
        async fn list_sandboxes(
            &self,
        ) -> crate::backend::Result<Vec<crate::protocol::SandboxInfo>> {
            self.inner.list_sandboxes().await
        }
        async fn remove_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            self.inner.remove_sandbox(id).await
        }
        async fn adopt_sandbox(
            &self,
            info: crate::protocol::SandboxInfo,
            opts: CreateOpts,
            rootfs: std::path::PathBuf,
        ) -> crate::backend::Result<()> {
            self.inner.adopt_sandbox(info, opts, rootfs).await
        }
        async fn stop_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            if self.fail_stop {
                return Err(crate::backend::BackendError::Internal(
                    "stop refused".into(),
                ));
            }
            self.inner.stop_sandbox(id).await
        }
        async fn start_sandbox(&self, id: &str) -> crate::backend::Result<()> {
            self.inner.start_sandbox(id).await
        }
        async fn count(&self) -> crate::backend::Result<usize> {
            self.inner.count().await
        }
        async fn kill_process(&self, sandbox_id: &str, pid: &str) -> crate::backend::Result<()> {
            self.inner.kill_process(sandbox_id, pid).await
        }
        async fn copy_to(
            &self,
            sandbox_id: &str,
            path: &str,
            chunks: tokio::sync::mpsc::Receiver<bytes::Bytes>,
        ) -> crate::backend::Result<()> {
            self.inner.copy_to(sandbox_id, path, chunks).await
        }
        async fn copy_from(
            &self,
            sandbox_id: &str,
            path: &str,
        ) -> crate::backend::Result<tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>>
        {
            self.inner.copy_from(sandbox_id, path).await
        }
        async fn top(
            &self,
            _sandbox_id: &str,
            ps_args: &[String],
        ) -> crate::backend::Result<crate::backend::ProcessTable> {
            if ps_args.iter().any(|a| a == TOP_REFUSED_ARG) {
                return Err(crate::backend::BackendError::InvalidArgument(
                    TOP_REFUSAL.into(),
                ));
            }
            Ok(crate::backend::ProcessTable {
                titles: vec!["PID".into(), "CMD".into()],
                rows: vec![vec![TOP_PID.into(), TOP_CMD.into()]],
            })
        }
        async fn stats(
            &self,
            _sandbox_id: &str,
        ) -> crate::backend::Result<crate::backend::GuestStats> {
            Ok(STATS_GUEST)
        }
        async fn create_snapshot(
            &self,
            sandbox_id: &str,
            label: &str,
        ) -> crate::backend::Result<crate::protocol::SnapshotInfo> {
            self.inner.create_snapshot(sandbox_id, label).await
        }
        async fn restore_snapshot(
            &self,
            sandbox_id: &str,
            snapshot_id: &str,
        ) -> crate::backend::Result<()> {
            self.inner.restore_snapshot(sandbox_id, snapshot_id).await
        }
        async fn list_snapshots(
            &self,
            sandbox_id: &str,
        ) -> crate::backend::Result<Vec<crate::protocol::SnapshotInfo>> {
            self.inner.list_snapshots(sandbox_id).await
        }
        async fn snapshot_owner(&self, snapshot_id: &str) -> crate::backend::Result<String> {
            self.inner.snapshot_owner(snapshot_id).await
        }
    }

    #[cfg(not(feature = "krunvm"))]
    const TEST_POLL_STEP: std::time::Duration = std::time::Duration::from_millis(100);
    #[cfg(not(feature = "krunvm"))]
    const TEST_POLL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    #[cfg(not(feature = "krunvm"))]
    const STATS_GUEST: crate::backend::GuestStats = crate::backend::GuestStats {
        cpu_nanos: 3_000_000_000,
        online_cpus: 2,
        mem_total_bytes: 512 * BYTES_PER_MIB,
        mem_available_bytes: 112 * BYTES_PER_MIB,
        pids: 9,
    };

    #[cfg(not(feature = "krunvm"))]
    const TEST_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
    #[cfg(not(feature = "krunvm"))]
    const HOST_MANAGER_MAX_SANDBOXES: usize = 4;

    #[cfg(not(feature = "krunvm"))]
    pub(crate) fn host_manager(output_cap: usize) -> Arc<SandboxManager> {
        host_manager_with(output_cap, /* fail_stop = */ false)
    }

    #[cfg(not(feature = "krunvm"))]
    fn host_manager_with(output_cap: usize, fail_stop: bool) -> Arc<SandboxManager> {
        use crate::backend::image::ImageStore;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        std::mem::forget(dir);
        let inner = crate::backend::krunvm::KrunvmBackend::with_image_store_for_test(path, store);
        let backend: Arc<dyn Backend> = Arc::new(HostBackend { inner, fail_stop });
        Arc::new(
            SandboxManager::new(
                backend,
                Arc::new(Broker::new()),
                HOST_MANAGER_MAX_SANDBOXES,
                /* allow_host_mounts = */ false,
                crate::config::NetworkBackendChoice::Smoltcp,
            )
            .with_output_cap(output_cap)
            .with_stop_grace(TEST_STOP_GRACE),
        )
    }

    #[cfg(not(feature = "krunvm"))]
    pub(crate) async fn create_with_command(
        mgr: &SandboxManager,
        command: &[&str],
        tty: bool,
    ) -> String {
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: command.iter().map(|part| part.to_string()).collect(),
            tty,
            ..Default::default()
        };
        mgr.create(req, "").await.expect("create").id
    }

    #[cfg(not(feature = "krunvm"))]
    pub(crate) fn concat(chunks: &[OutputChunk]) -> Vec<u8> {
        chunks
            .iter()
            .flat_map(|c| c.bytes.iter().copied())
            .collect()
    }

    const TEST_EPOCH_MILLIS: i64 = 1_800_000_000_000;

    fn millis(offset: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(TEST_EPOCH_MILLIS + offset).expect("valid time")
    }

    fn chunk(stream: RawStream, text: &str) -> OutputChunk {
        chunk_at(stream, text, millis(0))
    }

    fn chunk_at(stream: RawStream, text: &str, at: chrono::DateTime<chrono::Utc>) -> OutputChunk {
        OutputChunk {
            stream,
            bytes: bytes::Bytes::copy_from_slice(text.as_bytes()),
            at,
        }
    }

    #[test]
    fn given_a_burst_of_one_byte_writes_in_one_millisecond_when_buffered_then_chunks_stay_few_and_fast()
     {
        // Arrange
        const WRITES: usize = 200_000;
        const TIME_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
        let buffer = OutputBuffer::new(MAIN_OUTPUT_CAP * 4);
        let started = std::time::Instant::now();

        // Act
        for _ in 0..WRITES {
            buffer.push(chunk_at(RawStream::Stdout, "x", millis(1)));
        }

        // Assert
        let (replay, _) = buffer.subscribe();
        assert!(
            replay.len() <= WRITES / OUTPUT_MERGE_LIMIT + 1,
            "{}",
            replay.len()
        );
        assert_eq!(replay.iter().map(|c| c.bytes.len()).sum::<usize>(), WRITES);
        assert!(started.elapsed() < TIME_BUDGET, "{:?}", started.elapsed());
    }

    #[test]
    fn given_chunks_in_different_milliseconds_when_buffered_then_each_keeps_its_arrival_time() {
        // Arrange
        let buffer = OutputBuffer::new(MAIN_OUTPUT_CAP);

        // Act
        buffer.push(chunk_at(RawStream::Stdout, "a\n", millis(1)));
        buffer.push(chunk_at(RawStream::Stdout, "b\n", millis(2)));

        // Assert
        let (replay, _) = buffer.subscribe();
        assert_eq!(
            replay.iter().map(|c| c.at).collect::<Vec<_>>(),
            vec![millis(1), millis(2)]
        );
    }

    #[test]
    fn given_same_stream_chunks_in_one_millisecond_when_buffered_then_they_merge_and_the_live_copy_stays_whole()
     {
        // Arrange
        let buffer = OutputBuffer::new(MAIN_OUTPUT_CAP);
        let (_, mut live) = buffer.subscribe();

        // Act
        buffer.push(chunk_at(RawStream::Stdout, "a", millis(5)));
        buffer.push(chunk_at(RawStream::Stdout, "b", millis(5)));
        buffer.push(chunk_at(RawStream::Stderr, "c", millis(5)));

        // Assert
        let (replay, _) = buffer.subscribe();
        assert_eq!(
            replay,
            vec![
                chunk_at(RawStream::Stdout, "ab", millis(5)),
                chunk_at(RawStream::Stderr, "c", millis(5)),
            ]
        );
        assert_eq!(
            live.try_recv().unwrap(),
            chunk_at(RawStream::Stdout, "a", millis(5))
        );
        let state = buffer.state.lock().unwrap();
        assert_eq!(state.len, 3 + 2 * OUTPUT_CHUNK_OVERHEAD);
    }

    #[test]
    fn given_chunks_from_both_streams_when_buffered_then_replay_keeps_each_stream() {
        // Arrange
        let buffer = OutputBuffer::new(MAIN_OUTPUT_CAP);

        // Act
        buffer.push(chunk(RawStream::Stdout, "out\n"));
        buffer.push(chunk(RawStream::Stderr, "err\n"));

        // Assert
        let (replay, _) = buffer.subscribe();
        assert_eq!(
            replay,
            vec![
                chunk(RawStream::Stdout, "out\n"),
                chunk(RawStream::Stderr, "err\n")
            ]
        );
    }

    #[test]
    fn given_output_over_the_cap_when_buffered_then_the_oldest_bytes_go_and_streams_hold() {
        // Arrange
        let buffer = OutputBuffer::new(5 + 2 * OUTPUT_CHUNK_OVERHEAD);

        // Act
        buffer.push(chunk(RawStream::Stdout, "abc"));
        buffer.push(chunk(RawStream::Stderr, "def"));

        // Assert
        let (replay, _) = buffer.subscribe();
        assert_eq!(
            replay,
            vec![
                chunk(RawStream::Stdout, "bc"),
                chunk(RawStream::Stderr, "def")
            ]
        );
    }

    #[test]
    fn given_many_one_byte_writes_when_buffered_then_retained_chunks_stay_bounded() {
        // Arrange
        let cap = 10 * OUTPUT_CHUNK_OVERHEAD;
        let buffer = OutputBuffer::new(cap);

        // Act
        for _ in 0..10_000 {
            buffer.push(chunk(RawStream::Stdout, "x"));
        }

        // Assert
        let (replay, _) = buffer.subscribe();
        assert!(replay.len() <= cap / OUTPUT_CHUNK_OVERHEAD);
    }

    #[tokio::test]
    async fn given_a_subscriber_when_stderr_is_pushed_then_the_live_chunk_carries_stderr() {
        // Arrange
        let buffer = OutputBuffer::new(MAIN_OUTPUT_CAP);
        let (_, mut live) = buffer.subscribe();

        // Act
        buffer.push(chunk(RawStream::Stderr, "err\n"));

        // Assert
        assert_eq!(
            live.recv().await.unwrap(),
            chunk(RawStream::Stderr, "err\n")
        );
    }

    /// Poll the buffered output until it equals `expected`.
    #[cfg(not(feature = "krunvm"))]
    async fn wait_for_output(mgr: &SandboxManager, id: &str, expected: &str) {
        let deadline = std::time::Instant::now() + TEST_POLL_DEADLINE;
        loop {
            let (buffered, _) = mgr.subscribe_main_output(id).await.expect("subscribe");
            if concat(&buffered) == expected.as_bytes() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "output {:?} never became {expected:?}",
                String::from_utf8_lossy(&concat(&buffered))
            );
            tokio::time::sleep(TEST_POLL_STEP).await;
        }
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_stopped_sandbox_with_command_when_started_then_output_holds_a_second_line() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo run"], false).await;
        wait_for_output(&mgr, &id, "run\n").await;
        mgr.stop(&id).await.expect("stop");

        // Act
        mgr.start(&id).await.expect("start");

        // Assert
        wait_for_output(&mgr, &id, "run\nrun\n").await;
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_stopped_sandbox_with_command_when_restarted_then_output_holds_a_second_line() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo run"], false).await;
        wait_for_output(&mgr, &id, "run\n").await;
        mgr.stop(&id).await.expect("stop");

        // Act
        mgr.restart(&id).await.expect("restart");

        // Assert
        wait_for_output(&mgr, &id, "run\nrun\n").await;
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::pty(true, "tty\n")]
    #[case::pipe(false, "notty\n")]
    #[tokio::test]
    async fn given_tty_setting_when_command_tests_stdin_then_output_says_which(
        #[case] tty: bool,
        #[case] expected: &str,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);

        // Act
        let id = create_with_command(
            &mgr,
            &["sh", "-c", "test -t 0 && echo tty || echo notty"],
            tty,
        )
        .await;

        // Assert: a pty may add a carriage return before the newline.
        let deadline = std::time::Instant::now() + TEST_POLL_DEADLINE;
        loop {
            let (buffered, _) = mgr.subscribe_main_output(&id).await.expect("subscribe");
            let text = String::from_utf8_lossy(&concat(&buffered)).replace('\r', "");
            if text == expected {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "got {text:?}");
            tokio::time::sleep(TEST_POLL_STEP).await;
        }
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_spawned_main_process_when_record_read_then_pid_is_the_childs() {
        // Arrange: the shell prints its own pid, then waits.
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo $$; read x"], false).await;
        let pid = mgr.main_pid(&id).await.expect("pid");

        // Act / Assert
        wait_for_output(&mgr, &id, &format!("{pid}\n")).await;
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_main_process_exits_7_when_bridge_finishes_then_status_is_0x0700() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 7"], false).await;

        // Act
        let status = mgr.main_exit_status(&id).await.expect("status");

        // Assert
        assert_eq!(status, 0x0700);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_main_process_killed_when_bridge_finishes_then_status_is_0x0009() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        let pid: i32 = mgr.main_pid(&id).await.unwrap().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");

        // Act
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill");
        let status = mgr.main_exit_status(&id).await.expect("status");

        // Assert
        assert_eq!(status, 0x0009);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_cat_main_process_and_no_client_when_polled_then_it_stays_running() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["cat"], false).await;

        // Act / Assert: stdin stays open, so cat never ends.
        for _ in 0..20 {
            let info = mgr.get(&id).await.expect("get");
            assert_eq!(info.status, SandboxStatus::Running as i32);
            let ended = tokio::time::timeout(
                std::time::Duration::from_millis(1),
                mgr.main_exit_status(&id),
            )
            .await;
            assert!(ended.is_err(), "cat ended");
            tokio::time::sleep(TEST_POLL_STEP).await;
        }
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_output_before_attach_when_subscribed_later_then_earlier_and_newer_arrive_in_order()
     {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let gate_dir = tempfile::tempdir().expect("tempdir");
        let gate = gate_dir.path().join("go");
        let script = format!(
            "echo one; until [ -e {} ]; do sleep 0.05; done; echo two",
            gate.display()
        );
        let id = create_with_command(&mgr, &["sh", "-c", &script], false).await;
        wait_for_output(&mgr, &id, "one\n").await;

        // Act
        let (earlier, mut live) = mgr.subscribe_main_output(&id).await.expect("subscribe");
        std::fs::write(&gate, b"").expect("open the gate");
        let newer = tokio::time::timeout(TEST_POLL_DEADLINE, live.recv())
            .await
            .expect("live output")
            .expect("open");

        // Assert
        assert_eq!(concat(&earlier), b"one\n");
        assert_eq!(&newer.bytes[..], b"two\n");
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::below_cap(7, "0123456")]
    #[case::at_cap(8, "01234567")]
    #[case::above_cap(9, "12345678")]
    #[tokio::test]
    async fn given_small_buffer_cap_when_output_exceeds_it_then_only_newest_bytes_remain(
        #[case] written: usize,
        #[case] expected: &str,
    ) {
        // Arrange
        const CAP: usize = 8 + OUTPUT_CHUNK_OVERHEAD;
        let mgr = host_manager(CAP);
        let text = &"0123456789"[..written];

        // Act
        let id = create_with_command(&mgr, &["printf", "%s", text], false).await;
        mgr.main_exit_status(&id).await.expect("status");

        // Assert
        let (buffered, _) = mgr.subscribe_main_output(&id).await.expect("subscribe");
        assert_eq!(String::from_utf8(concat(&buffered)).unwrap(), expected);
    }
    // ----- exited state -------------------------------------------------

    #[cfg(not(feature = "krunvm"))]
    const SIGKILL_EXIT_CODE: i32 = 137;
    #[cfg(not(feature = "krunvm"))]
    const SIGTERM_EXIT_CODE: i32 = 143;
    /// Poll until the sandbox reports `want`.
    #[cfg(not(feature = "krunvm"))]
    async fn wait_for_status(mgr: &SandboxManager, id: &str, want: SandboxStatus) {
        let deadline = std::time::Instant::now() + TEST_POLL_DEADLINE;
        while status_of(mgr, id).await != want {
            assert!(
                std::time::Instant::now() < deadline,
                "never became {want:?}"
            );
            tokio::time::sleep(TEST_POLL_STEP).await;
        }
    }

    #[cfg(not(feature = "krunvm"))]
    async fn exit_code_of(mgr: &SandboxManager, id: &str) -> Option<i32> {
        mgr.get(id).await.expect("get").exit_code
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::exit_7("exit 7", 7)]
    #[case::exit_0("exit 0", 0)]
    #[case::killed("kill -9 $$", SIGKILL_EXIT_CODE)]
    #[tokio::test]
    async fn given_command_that_ends_when_it_ends_then_sandbox_is_exited_with_its_code(
        #[case] script: &str,
        #[case] code: i32,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);

        // Act
        let id = create_with_command(&mgr, &["sh", "-c", script], false).await;

        // Assert
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        assert_eq!(exit_code_of(&mgr, &id).await, Some(code));
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_auto_remove_when_main_process_ends_then_sandbox_is_removed() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["true".into()],
            auto_remove: true,
            ..Default::default()
        };

        // Act
        let id = mgr.create(req, "").await.expect("create").id;

        // Assert
        let deadline = std::time::Instant::now() + TEST_POLL_DEADLINE;
        while mgr.get(&id).await.is_ok() {
            assert!(std::time::Instant::now() < deadline, "never removed");
            tokio::time::sleep(TEST_POLL_STEP).await;
        }
        assert!(matches!(
            mgr.remove(&id).await,
            Err(ApiError::SandboxNotFound(_))
        ));
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_command_that_ends_when_restarted_then_times_follow_the_runs() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 7"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        let first = mgr.get(&id).await.expect("get");

        // Act
        let restarted = mgr.restart(&id).await.expect("restart");

        // Assert
        let timestamp = |t: &prost_types::Timestamp| (t.seconds, t.nanos);
        let first_start = timestamp(&first.started_at.expect("first started_at"));
        let first_finish = timestamp(&first.finished_at.expect("first finished_at"));
        let second_start = timestamp(&restarted.started_at.expect("second started_at"));
        assert_ne!(second_start, first_start);
        assert!(second_start >= first_finish);
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        let second = mgr.get(&id).await.expect("get");
        assert!(timestamp(&second.finished_at.expect("second finished_at")) >= second_start);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_finished_sandbox_when_restarted_then_it_keeps_the_previous_finish_time() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "sleep 5"], false).await;
        mgr.stop_within(&id, Some(0)).await.expect("stop");
        let stopped = mgr.get(&id).await.expect("get");

        // Act
        let restarted = mgr.start(&id).await.expect("start");

        // Assert
        assert_eq!(restarted.finished_at, stopped.finished_at);
        assert!(restarted.finished_at.is_some());
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::with_command(&["sleep", "5"], true)]
    #[case::without_command(&[], false)]
    #[tokio::test]
    async fn given_command_or_not_when_created_then_started_at_is_set_only_with_a_command(
        #[case] command: &[&str],
        #[case] has_started_at: bool,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: command.iter().map(|part| part.to_string()).collect(),
            ..Default::default()
        };

        // Act
        let created = mgr.create(req, "").await.expect("create");

        // Assert
        let got = mgr.get(&created.id).await.expect("get");
        assert_eq!(created.started_at.is_some(), has_started_at);
        assert_eq!(created.started_at, got.started_at);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_sandbox_without_command_when_started_again_then_started_at_stays_unset() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            ..Default::default()
        };
        let id = mgr.create(req, "").await.expect("create").id;
        mgr.stop(&id).await.expect("stop");

        // Act
        let started = mgr.start(&id).await.expect("start");

        // Assert
        assert!(started.started_at.is_none());
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::auto_remove(true)]
    #[case::kept(false)]
    #[tokio::test]
    async fn given_running_sandbox_when_stopped_then_only_auto_remove_removes_it(
        #[case] auto_remove: bool,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["sleep".into(), "30".into()],
            auto_remove,
            ..Default::default()
        };
        let id = mgr.create(req, "").await.expect("create").id;

        // Act
        let stopped = mgr.stop_within(&id, Some(0)).await.expect("stop");

        // Assert
        assert_eq!(stopped.status, SandboxStatus::Stopped as i32);
        assert_eq!(mgr.get(&id).await.is_ok(), !auto_remove);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_auto_remove_sandbox_when_restarted_then_it_is_kept_running() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["sleep".into(), "30".into()],
            auto_remove: true,
            ..Default::default()
        };
        let id = mgr.create(req, "").await.expect("create").id;

        // Act
        let restarted = mgr.restart_within(&id, Some(0)).await.expect("restart");

        // Assert
        assert_eq!(restarted.status, SandboxStatus::Running as i32);
        assert!(mgr.get(&id).await.is_ok());
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_sandbox_started_again_after_its_run_ended_when_auto_remove_runs_then_it_stays() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 0"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        let ended_pid = mgr.main_pid(&id).await.expect("main pid");
        mgr.start(&id).await.expect("start again");

        // Act
        auto_remove_ended(&mgr.teardown(), &id, &ended_pid).await;

        // Assert
        assert!(mgr.get(&id).await.is_ok());
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::default(None, Some(TEST_STOP_GRACE))]
    #[case::immediate(Some(0), Some(std::time::Duration::ZERO))]
    #[case::seconds(Some(30), Some(std::time::Duration::from_secs(30)))]
    #[case::forever(Some(-1), None)]
    fn given_requested_grace_when_resolved_then_default_zero_seconds_or_unlimited(
        #[case] requested: Option<i32>,
        #[case] want: Option<std::time::Duration>,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);

        // Act and Assert
        assert_eq!(mgr.grace_period(requested), want);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_term_ignoring_command_when_stopped_with_zero_grace_then_it_is_killed_at_once() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(
            &mgr,
            &[
                "sh",
                "-c",
                "trap '' TERM; echo up; while :; do read x; done",
            ],
            false,
        )
        .await;
        wait_for_output(&mgr, &id, "up\n").await;
        let started = std::time::Instant::now();

        // Act
        mgr.stop_within(&id, Some(0)).await.expect("stop");

        // Assert
        assert!(started.elapsed() < TEST_STOP_GRACE);
        assert_eq!(exit_code_of(&mgr, &id).await, Some(SIGKILL_EXIT_CODE));
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::default_is_kill(0, SIGKILL_EXIT_CODE)]
    #[case::term(SIGTERM, SIGTERM_EXIT_CODE)]
    #[tokio::test]
    async fn given_running_command_when_main_process_signalled_then_sandbox_exits_with_signal_code(
        #[case] signal: i32,
        #[case] code: i32,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;

        // Act
        mgr.signal_main_process(&id, signal).await.expect("signal");

        // Assert
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        assert_eq!(exit_code_of(&mgr, &id).await, Some(code));
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::negative(-1)]
    #[case::past_max(65)]
    #[tokio::test]
    async fn given_signal_out_of_range_when_main_process_signalled_then_invalid_request(
        #[case] signal: i32,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;

        // Act
        let err = mgr
            .signal_main_process(&id, signal)
            .await
            .expect_err("out of range");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err}");
        assert!(err.to_string().contains("invalid signal"), "{err}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_exited_sandbox_when_main_process_signalled_then_not_running_error() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 0"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;

        // Act
        let err = mgr.signal_main_process(&id, 0).await.expect_err("exited");

        // Assert
        assert!(err.to_string().contains("is not running"), "{err}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_stopped_sandbox_when_main_process_signalled_then_not_running_error() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        mgr.stop(&id).await.expect("stop");

        // Act
        let err = mgr.signal_main_process(&id, 0).await.expect_err("stopped");

        // Assert
        assert!(err.to_string().contains("is not running"), "{err}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_sandbox_without_command_when_main_process_signalled_then_no_main_process_error()
    {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = mgr
            .create(
                CreateSandboxRequest {
                    image: "alpine".into(),
                    ..Default::default()
                },
                "",
            )
            .await
            .expect("create")
            .id;

        // Act
        let err = mgr.signal_main_process(&id, 0).await.expect_err("no main");

        // Assert
        assert!(err.to_string().contains("no main process"), "{err}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_exited_sandbox_when_stopped_then_it_is_stopped_and_keeps_its_code() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 7"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;

        // Act
        mgr.stop(&id).await.expect("stop");

        // Assert
        assert_eq!(status_of(&mgr, &id).await, SandboxStatus::Stopped);
        assert_eq!(exit_code_of(&mgr, &id).await, Some(7));
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::terminated("echo up; read x", SIGTERM_EXIT_CODE)]
    #[case::forced("trap '' TERM; echo up; while :; do read x; done", SIGKILL_EXIT_CODE)]
    #[tokio::test]
    async fn given_running_command_when_stopped_then_it_is_stopped_with_the_signal_code(
        #[case] script: &str,
        #[case] code: i32,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", script], false).await;
        wait_for_output(&mgr, &id, "up\n").await;

        // Act
        mgr.stop(&id).await.expect("stop");

        // Assert
        assert_eq!(status_of(&mgr, &id).await, SandboxStatus::Stopped);
        assert_eq!(exit_code_of(&mgr, &id).await, Some(code));
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_exited_sandbox_when_restarted_then_code_clears_and_the_new_run_sets_it() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 5"], false).await;
        let send_go = |mgr: Arc<SandboxManager>, id: String| async move {
            let stdin = mgr.main_io(&id).await.expect("io").stdin.expect("stdin");
            stdin
                .send(bytes::Bytes::from_static(b"go\n"))
                .await
                .expect("send");
        };
        send_go(Arc::clone(&mgr), id.clone()).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        assert_eq!(exit_code_of(&mgr, &id).await, Some(5));

        // Act
        mgr.restart(&id).await.expect("restart");

        // Assert
        assert_eq!(status_of(&mgr, &id).await, SandboxStatus::Running);
        assert_eq!(exit_code_of(&mgr, &id).await, None);
        send_go(Arc::clone(&mgr), id.clone()).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
        assert_eq!(exit_code_of(&mgr, &id).await, Some(5));
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_command_exited_before_subscribe_when_read_later_then_output_and_status_remain() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo hi; exit 3"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;

        // Act
        let (buffered, _) = mgr.subscribe_main_output(&id).await.expect("subscribe");
        let status = mgr.main_exit_status(&id).await.expect("status");

        // Assert
        assert_eq!(concat(&buffered), b"hi\n");
        assert_eq!(status, 3 << WAIT_STATUS_EXIT_SHIFT);
    }

    #[cfg(not(feature = "krunvm"))]
    const TOP_PID: &str = "42";
    #[cfg(not(feature = "krunvm"))]
    const TOP_CMD: &str = "sleep 600";
    #[cfg(not(feature = "krunvm"))]
    const TOP_REFUSED_ARG: &str = "--sort=pid";
    #[cfg(not(feature = "krunvm"))]
    const TOP_REFUSAL: &str = "unsupported ps option: --sort=pid";

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_running_sandbox_when_top_then_the_guest_table_is_returned() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;

        // Act
        let table = mgr.top(&id, &[]).await.expect("top");

        // Assert
        assert_eq!(table.titles, ["PID", "CMD"]);
        assert_eq!(table.rows, [[TOP_PID, TOP_CMD]]);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_stopped_sandbox_when_top_then_docker_not_running_error() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        mgr.stop(&id).await.expect("stop");

        // Act
        let err = mgr.top(&id, &[]).await.expect_err("not running");

        // Assert
        assert!(
            matches!(&err, ApiError::InvalidRequest(m) if *m == format!("Container {id} is not running"))
        );
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_backend_that_refuses_the_ps_options_when_top_then_invalid_request() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;

        // Act
        let err = mgr
            .top(&id, &[TOP_REFUSED_ARG.to_string()])
            .await
            .expect_err("refused");

        // Assert
        assert!(
            matches!(&err, ApiError::InvalidRequest(m) if m == TOP_REFUSAL),
            "{err:?}"
        );
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::none_configured(0, STATS_GUEST.mem_total_bytes)]
    #[case::configured(256, 256 * BYTES_PER_MIB)]
    #[tokio::test]
    async fn given_a_memory_setting_when_stats_then_used_memory_and_the_limit_follow_it(
        #[case] memory_mb: u32,
        #[case] limit: u64,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["sh".into(), "-c".into(), "read x".into()],
            resources: Some(PbResourceLimits {
                memory_mb,
                ..Default::default()
            }),
            ..Default::default()
        };
        let id = mgr.create(req, "").await.expect("create").id;

        // Act
        let stats = mgr.stats(&id).await.expect("stats");

        // Assert
        assert_eq!(stats.mem_limit_bytes, limit);
        assert_eq!(stats.mem_used_bytes, 400 * BYTES_PER_MIB);
        assert_eq!(stats.cpu_nanos, STATS_GUEST.cpu_nanos);
        assert_eq!(stats.pids, STATS_GUEST.pids);
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::stopped_meanwhile(true)]
    #[case::still_running(false)]
    #[tokio::test]
    async fn given_a_backend_failure_when_stats_then_a_stopped_sandbox_is_not_running(
        #[case] stop: bool,
    ) {
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        if stop {
            mgr.stop(&id).await.expect("stop");
        }

        let err = mgr
            .stats_failure(&id, BackendError::Exec("agent hung up".into()))
            .await;

        assert_eq!(matches!(err, ApiError::InvalidRequest(_)), stop, "{err:?}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_sandbox_that_is_not_a_microvm_when_stats_then_an_invalid_request() {
        let mgr = build_manager(2);
        let id = mgr.create(create_req("alpine"), "").await.unwrap().id;

        let err = mgr.stats(&id).await.expect_err("no agent");

        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_stopped_sandbox_when_stats_then_docker_not_running_error() {
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        mgr.stop(&id).await.expect("stop");

        let err = mgr.stats(&id).await.expect_err("not running");

        let want = format!("Container {id} is not running");
        assert!(
            matches!(&err, ApiError::InvalidRequest(m) if *m == want),
            "{err:?}"
        );
    }

    /// Wait on `id` in a task and return its handle.
    #[cfg(not(feature = "krunvm"))]
    fn spawn_wait(mgr: &Arc<SandboxManager>, id: &str) -> tokio::task::JoinHandle<Result<i32>> {
        let (mgr, id) = (Arc::clone(mgr), id.to_string());
        tokio::spawn(async move { mgr.wait(&id).await })
    }

    #[cfg(not(feature = "krunvm"))]
    async fn send_line(mgr: &SandboxManager, id: &str, line: &'static [u8]) {
        let stdin = mgr.main_io(id).await.expect("io").stdin.expect("stdin");
        stdin
            .send(bytes::Bytes::from_static(line))
            .await
            .expect("send");
    }

    #[cfg(not(feature = "krunvm"))]
    #[rstest::rstest]
    #[case::auto_remove(true)]
    #[case::kept(false)]
    #[tokio::test]
    async fn given_a_command_that_exits_4_when_waited_then_4_is_returned_even_if_it_is_removed(
        #[case] auto_remove: bool,
    ) {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["sh".into(), "-c".into(), "read x; exit 4".into()],
            auto_remove,
            ..Default::default()
        };
        let id = mgr.create(req, "").await.expect("create").id;
        let waiter = spawn_wait(&mgr, &id);

        // Act
        send_line(&mgr, &id, b"go\n").await;
        let got = waiter.await.expect("join").expect("wait");

        // Assert
        assert_eq!(got, 4);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_sandbox_without_command_when_waited_then_it_returns_0_only_after_stop() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &[], false).await;
        let mut waiter = tokio::spawn({
            let (mgr, id) = (Arc::clone(&mgr), id.clone());
            async move { (mgr.wait(&id).await, status_of(&mgr, &id).await) }
        });
        let early = tokio::time::timeout(WAIT_POLL * 3, &mut waiter).await;
        assert!(early.is_err(), "wait returned while the sandbox ran");

        // Act
        mgr.stop(&id).await.expect("stop");
        let (code, status_at_return) = waiter.await.expect("join");

        // Assert
        assert_eq!(code.expect("wait"), 0);
        assert_eq!(status_at_return, SandboxStatus::Stopped);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_an_exited_sandbox_restarted_to_exit_m_when_waited_then_m_is_returned() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x; exit $x"], false).await;
        send_line(&mgr, &id, b"5\n").await;
        assert_eq!(mgr.wait(&id).await.expect("first wait"), 5);
        mgr.restart(&id).await.expect("restart");
        let waiter = spawn_wait(&mgr, &id);

        // Act
        send_line(&mgr, &id, b"9\n").await;
        let got = waiter.await.expect("join").expect("wait");

        // Assert
        assert_eq!(got, 9);
    }

    #[cfg(not(feature = "krunvm"))]
    async fn next_actions(
        reader: &mut crate::sandbox::events::EventReader,
        count: usize,
    ) -> Vec<(EventAction, Option<i32>)> {
        let mut out = Vec::new();
        for _ in 0..count {
            let next = tokio::time::timeout(TEST_POLL_DEADLINE, reader.next())
                .await
                .expect("event in time")
                .expect("event");
            out.push((next.event.action, next.event.exit_code));
        }
        out
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_short_command_when_created_ended_stopped_and_removed_then_events_come_in_order()
     {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let mut reader = mgr.events().subscribe(None);
        let id = create_with_command(&mgr, &["sh", "-c", "exit 3"], false).await;
        wait_for_status(&mgr, &id, SandboxStatus::Exited).await;

        // Act
        mgr.stop(&id).await.expect("stop");
        mgr.remove(&id).await.expect("remove");
        let got = next_actions(&mut reader, 5).await;

        // Assert
        let want = [
            (EventAction::Create, None),
            (EventAction::Start, None),
            (EventAction::Die, Some(3)),
            (EventAction::Stop, None),
            (EventAction::Destroy, None),
        ];
        assert_eq!(got, want);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_running_sandbox_when_stopped_then_die_comes_before_stop() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sleep", "30"], false).await;
        let mut reader = mgr
            .events()
            .subscribe(Some(std::time::SystemTime::UNIX_EPOCH));

        // Act
        mgr.stop(&id).await.expect("stop");
        let got = next_actions(&mut reader, 4).await;

        // Assert
        let actions: Vec<EventAction> = got.iter().map(|(action, _)| *action).collect();
        assert_eq!(
            actions,
            [
                EventAction::Create,
                EventAction::Start,
                EventAction::Die,
                EventAction::Stop
            ]
        );
    }

    #[cfg(not(feature = "krunvm"))]
    async fn history(mgr: &SandboxManager, count: usize) -> Vec<(EventAction, Option<i32>)> {
        let mut reader = mgr
            .events()
            .subscribe(Some(std::time::SystemTime::UNIX_EPOCH));
        next_actions(&mut reader, count).await
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_running_sandbox_when_force_removed_then_die_137_comes_before_destroy() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sleep", "30"], false).await;

        // Act
        mgr.remove(&id).await.expect("remove");

        // Assert
        let want = [
            (EventAction::Create, None),
            (EventAction::Start, None),
            (EventAction::Die, Some(137)),
            (EventAction::Destroy, None),
        ];
        assert_eq!(history(&mgr, 4).await, want);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_running_sandbox_when_its_timeout_expires_then_die_comes_before_destroy() {
        // Arrange
        let mgr = build_manager(1);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            resources: Some(PbResourceLimits {
                timeout_seconds: 1,
                ..Default::default()
            }),
            ..Default::default()
        };

        // Act
        mgr.create(req, "").await.expect("create");

        // Assert
        let got = history(&mgr, 4).await;
        assert_eq!(got[2], (EventAction::Die, Some(137)));
        assert_eq!(got[3].0, EventAction::Destroy);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_command_that_cannot_run_when_created_then_start_die_126_and_destroy_follow() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let req = CreateSandboxRequest {
            image: "alpine".into(),
            command: vec!["/nonexistent/ward-test-command".into()],
            ..Default::default()
        };

        // Act
        mgr.create(req, "").await.expect_err("exec fails");

        // Assert
        let want = [
            (EventAction::Create, None),
            (EventAction::Start, None),
            (EventAction::Die, Some(126)),
            (EventAction::Destroy, None),
        ];
        assert_eq!(history(&mgr, 4).await, want);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_command_that_vanished_when_restarted_then_the_failed_run_ends_with_die_126() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").expect("write");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let id = create_with_command(&mgr, &[script.to_str().unwrap()], false).await;
        mgr.stop(&id).await.expect("stop");
        drop(dir);

        // Act
        mgr.start(&id).await.expect_err("exec fails");

        // Assert
        let got = history(&mgr, 6).await;
        assert_eq!(
            &got[4..],
            [(EventAction::Start, None), (EventAction::Die, Some(126))]
        );
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_backend_that_cannot_stop_when_stopping_then_die_is_reported_for_the_failed_sandbox()
     {
        // Arrange
        let mgr = host_manager_with(MAIN_OUTPUT_CAP, /* fail_stop = */ true);
        let id = create_with_command(&mgr, &["sleep", "30"], false).await;

        // Act
        mgr.stop(&id).await.expect_err("stop refused");

        // Assert
        let got = history(&mgr, 3).await;
        assert_eq!(got[2].0, EventAction::Die);
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_a_removed_sandbox_when_an_earlier_record_reports_then_the_event_is_kept() {
        // Arrange
        let mgr = host_manager(MAIN_OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sleep", "30"], false).await;
        let meta = mgr.entries.read().await.get(&id).unwrap().meta.clone();
        mgr.remove(&id).await.expect("remove");

        // Act
        mgr.emit_for(&meta, EventAction::Stop, None).await;

        // Assert
        let got = history(&mgr, 5).await;
        assert_eq!(got[4].0, EventAction::Stop);
    }
}
