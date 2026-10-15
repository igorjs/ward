// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! MicroVM backend using libkrun.
//!
//! All `unsafe` calls to the libkrun C ABI are confined to this module.
//! FFI declarations live in `super::krun_ffi` (hand-maintained, no
//! `krun-sys` crate, no bindgen). The public API is fully safe Rust.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::image::ImageStore;
use super::{Backend, BackendError, ProcessHandle, Result};
use crate::config::NetworkBackendChoice;
#[cfg(feature = "krunvm")]
use crate::egress::EgressProxy;
#[cfg(feature = "krunvm")]
use crate::protocol::EgressMode;
use crate::protocol::{CreateOpts, SandboxInfo, SandboxStatus, SnapshotInfo};

/// vsock port the guest agent listens on; must match `ward-agent`.
/// Only referenced by the krunvm-gated FFI wrappers.
#[allow(dead_code)]
const AGENT_VSOCK_PORT: u32 = 1024;

/// MAC address assigned to the guest's virtio-net device for the smoltcp
/// backend. Locally administered (0x02 prefix), distinct from
/// `INTERFACE_HARDWARE_ADDR` on the host side of `ward-net`'s `Stack`.
/// `krun_add_net_unixgram` requires a real, non-null MAC: confirmed
/// empirically that a null `c_mac` segfaults inside libkrun rather than
/// having it assign one itself, despite `libkrun.h` giving no indication
/// either way. Only referenced by the krunvm-gated FFI wrappers.
#[cfg(feature = "krunvm")]
const SMOLTCP_GUEST_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];

/// Path of the agent binary inside every sandbox rootfs.
/// Only referenced by the krunvm-gated FFI wrappers.
#[allow(dead_code)]
const AGENT_GUEST_PATH: &str = "/ward-agent";

/// Permissions of the agent binary in the rootfs (rwxr-xr-x).
#[cfg(feature = "krunvm")]
const AGENT_FILE_MODE: u32 = 0o755;

/// Delay before the stub exec path's scripted process signals completion.
/// A real caller's follow-up RPCs (kill_process, write_stdin, a second
/// stream_output call) need a realistic window to reach the manager before
/// the process "exits" and its bookkeeping is eligible for reaping; this
/// mirrors how an actual process takes measurable wall-clock time to run,
/// rather than completing before the exec response has even been handled.
/// Only referenced by the stub (non-krunvm) exec path.
#[cfg(not(feature = "krunvm"))]
const STUB_EXIT_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Timeout for connecting to the guest agent over its vsock-bridged Unix
/// socket. The socket exists once the microVM has finished booting, so a
/// healthy connect resolves almost instantly; this bounds it in case the
/// guest agent is wedged and never accepts.
/// Only referenced by the krunvm-gated exec path; exercised directly by
/// the tests below.
#[allow(dead_code)]
const AGENT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Wrap a guest-agent socket connect attempt in `AGENT_CONNECT_TIMEOUT`,
/// mapping either failure mode to `BackendError::Exec`. Accepts the connect
/// as a future rather than performing it directly so tests can substitute
/// one that never resolves: unlike a TCP handshake, a Unix-domain connect
/// either succeeds immediately or is refused outright once its listener's
/// backlog is full, so a genuine OS-level hang isn't reproducible here.
/// Only referenced by the krunvm-gated exec path; exercised directly by
/// the tests below.
#[allow(dead_code)]
async fn connect_agent_socket_with_timeout<F>(
    connect: F,
    sock: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<tokio::net::UnixStream>
where
    F: std::future::Future<Output = std::io::Result<tokio::net::UnixStream>>,
{
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(BackendError::Exec(format!(
            "connect to guest agent at {}: {e}",
            sock.display()
        ))),
        Err(_) => Err(BackendError::Exec(format!(
            "connect to guest agent at {}: timed out after {timeout:?}",
            sock.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// libc bindings used only by the krunvm boot path
// ---------------------------------------------------------------------------
//
// macOS-only: renamex_np(2) with RENAME_SWAP atomically swaps two
// existing paths. Used by extract_rootfs to close the SEC-014 residual
// on Darwin (Linux uses rustix::fs::renameat_with(RENAME_EXCHANGE) for
// the same effect). Declared inline rather than pulling in `libc`,
// matching the minimalism precedent set by the `write(2)` declaration
// above and `kill(2)` in ward-agent.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn renamex_np(
        from: *const std::ffi::c_char,
        to: *const std::ffi::c_char,
        flags: std::ffi::c_uint,
    ) -> std::ffi::c_int;
}
/// RENAME_SWAP flag for renamex_np (from `<sys/stdio.h>` on Darwin).
/// Swaps two existing paths atomically.
#[cfg(target_os = "macos")]
const RENAME_SWAP: std::ffi::c_uint = 0x0000_0002;

// ---------------------------------------------------------------------------
// Per-sandbox state
// ---------------------------------------------------------------------------

struct SandboxState {
    info: SandboxInfo,
    /// Host process running this sandbox's microVM. `None` in stub builds.
    #[cfg(feature = "krunvm")]
    shim: Option<super::vmshim::ShimProcess>,
    /// Live smoltcp stack task. `None` when egress mode is Deny, when the
    /// network backend is `None`, or when the `krunvm` feature is
    /// disabled.
    #[cfg(feature = "krunvm")]
    smoltcp: Option<ward_net::smoltcp_backend::SmoltcpHandle>,
    /// Egress policy proxy backing the smoltcp datapath's per-flow
    /// allow/deny check. Kept alive for the sandbox's lifetime: the
    /// spawned Stack task holds its own clone of this `Arc` inside the
    /// egress-check closure. `None` for Deny-mode sandboxes.
    #[cfg(feature = "krunvm")]
    egress: Option<Arc<EgressProxy>>,
}

// Manual impl instead of a derive: SmoltcpHandle wraps a JoinHandle and an
// mpsc::Sender that don't implement Debug, so smoltcp is reported by
// presence only.
impl std::fmt::Debug for SandboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("SandboxState");
        s.field("info", &self.info);
        #[cfg(feature = "krunvm")]
        s.field("shim", &self.shim)
            .field("smoltcp_active", &self.smoltcp.is_some())
            .field("egress_active", &self.egress.is_some());
        s.finish()
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Manages multiple krunvm sandboxes.
///
/// Each sandbox corresponds to one libkrun microVM context.  All unsafe krun
/// FFI calls are isolated inside the private helpers of this struct.
/// Per-process bookkeeping kept in `KrunvmBackend::processes`. The
/// sandbox_id is what enables SEC-017/018: kill_process refuses to
/// signal a pid whose owner doesn't match the caller's claim.
#[derive(Debug)]
struct ProcessRecord {
    sandbox_id: String,
    kill_tx: tokio::sync::mpsc::Sender<()>,
}

#[derive(Debug)]
pub struct KrunvmBackend {
    sandboxes: Arc<RwLock<HashMap<String, SandboxState>>>,
    /// Snapshot metadata keyed by snapshot_id. In stub mode this is a
    /// plain in-memory map; the real backend will pair this with on-disk
    /// libkrun checkpoint state under data_dir/snapshots/.
    snapshots: Arc<RwLock<HashMap<String, SnapshotInfo>>>,
    data_dir: std::path::PathBuf,
    /// Per-process kill records, keyed by pid. Each record carries the
    /// pid's owning sandbox_id alongside the kill channel so
    /// `kill_process` can verify the caller-claimed sandbox owns the
    /// pid before signalling. Populated by the real exec path; empty
    /// in stub builds, where killing is a no-op.
    ///
    /// SEC-017/018: storing sandbox_id on the value side closes the
    /// defence-in-depth gap where a future code path that calls
    /// `Backend::kill_process` directly (without going through the
    /// manager's ownership check) could signal a pid belonging to a
    /// different sandbox.
    processes: Arc<RwLock<HashMap<String, ProcessRecord>>>,
    /// Which network backend to use when spawning sandboxes. Defaults to
    /// `Smoltcp` (ADR-019, ADR-020); the only other choice is `None` (no
    /// egress). Only read under `--features krunvm`; in stub builds the
    /// field is stored for symmetry but never branched on.
    #[cfg_attr(not(feature = "krunvm"), allow(dead_code))]
    network_backend: NetworkBackendChoice,
    /// Content-addressable OCI image cache. Populated on first pull of a
    /// given manifest digest; subsequent sandboxes for the same image skip
    /// the registry pull and hardlink from the cached rootfs instead.
    image_store: Arc<ImageStore>,
}

impl KrunvmBackend {
    /// Production constructor. Defaults `max_cached_images` to 64,
    /// matching `Config`'s default. Daemon callers thread the operator's
    /// `WARD_MAX_CACHED_IMAGES` value through `new_with_network` instead
    /// so the env var has a single home (in `Config::from_env`) rather
    /// than being read in two places that can drift.
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        Self::new_with_network(data_dir, NetworkBackendChoice::default(), 64)
    }

    /// Construct with an explicit network backend choice + cache bound.
    /// The daemon's startup path calls this with values resolved from
    /// `Config` (which reads `WARD_NETWORK_BACKEND` and
    /// `WARD_MAX_CACHED_IMAGES`). Tests and embedded callers that do
    /// not need either knob can use `new` for the defaults.
    pub fn new_with_network(
        data_dir: std::path::PathBuf,
        network_backend: NetworkBackendChoice,
        max_cached_images: usize,
    ) -> Self {
        let cache_dir = data_dir.join("cache").join("images");
        let image_store = Arc::new(ImageStore::new(cache_dir, max_cached_images));
        Self::new_internal(data_dir, network_backend, image_store)
    }

    /// Internal constructor used by the public constructors and by tests that
    /// need to inject a fake image store (offline puller).
    fn new_internal(
        data_dir: std::path::PathBuf,
        network_backend: NetworkBackendChoice,
        image_store: Arc<ImageStore>,
    ) -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
            processes: Arc::new(RwLock::new(HashMap::new())),
            network_backend,
            image_store,
        }
    }

    /// Constructor for tests that need to inject a custom `ImageStore`
    /// (e.g. an offline `FakePuller`). Available in all builds so
    /// integration-test harnesses in `tests/` can use it without a
    /// `#[cfg(test)]` restriction; production callers should prefer
    /// `new` or `new_with_network`.
    pub fn with_image_store_for_test(
        data_dir: std::path::PathBuf,
        image_store: Arc<ImageStore>,
    ) -> Self {
        Self::new_internal(data_dir, NetworkBackendChoice::default(), image_store)
    }
}

#[async_trait::async_trait]
impl Backend for KrunvmBackend {
    /// Create a new sandbox and start the microVM.
    ///
    /// Under `--features krunvm` this starts a VM host process for the
    /// sandbox (see `vmshim`) and returns once the guest agent answers.
    /// `remove_sandbox` stops that process.
    async fn create_sandbox(&self, id: String, opts: &CreateOpts) -> Result<SandboxInfo> {
        let rootfs = self.sandbox_rootfs(&id);

        if let Some(snapshot_id) = &opts.from_snapshot {
            // Seed the rootfs from a snapshot if requested. The snapshot must
            // exist; its archived filesystem becomes this sandbox's starting
            // state (host-side; the VM then boots from it).
            if !self.snapshots.read().await.contains_key(snapshot_id) {
                return Err(BackendError::NotFound(snapshot_id.clone()));
            }
            let archive = self.snapshot_dir(snapshot_id).join("rootfs.tar");
            let dest = rootfs.clone();
            tokio::task::spawn_blocking(move || extract_rootfs(&archive, &dest))
                .await
                .map_err(|e| BackendError::Internal(format!("from_snapshot task: {e}")))??;
        } else if !opts.image.is_empty() {
            // Pull the image from the content-addressable cache (or the
            // registry on cache miss) and hardlink it into the sandbox rootfs.
            // This is the standard path for every `ward create` invocation.
            self.image_store
                .prepare_sandbox_rootfs(&opts.image, &rootfs)
                .await?;
        }

        #[cfg(feature = "krunvm")]
        let booted = self.boot_vm(&id, opts, &rootfs).await?;

        let now = std::time::SystemTime::now();
        let info = SandboxInfo {
            id: id.clone(),
            status: if cfg!(feature = "krunvm") {
                SandboxStatus::Running
            } else {
                SandboxStatus::Creating
            },
            image: opts.image.clone(),
            created_at: now,
            ip_address: None,
            resources: opts.resources.clone(),
            expires_at: if opts.resources.timeout_seconds > 0 {
                Some(now + std::time::Duration::from_secs(opts.resources.timeout_seconds))
            } else {
                None
            },
            // The backend has no concept of tokens; the manager layer
            // mints and stores the real token, then overwrites this.
            token: String::new(),
        };

        let state = SandboxState {
            info: info.clone(),
            #[cfg(feature = "krunvm")]
            shim: Some(booted.shim),
            #[cfg(feature = "krunvm")]
            smoltcp: booted.smoltcp,
            #[cfg(feature = "krunvm")]
            egress: booted.egress,
        };

        self.sandboxes.write().await.insert(id, state);
        Ok(info)
    }

    /// Retrieve sandbox info by ID.
    async fn get_sandbox(&self, id: &str) -> Result<SandboxInfo> {
        self.sandboxes
            .read()
            .await
            .get(id)
            .map(|s| s.info.clone())
            .ok_or_else(|| BackendError::NotFound(id.to_string()))
    }

    /// List all sandboxes.
    async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .values()
            .map(|s| s.info.clone())
            .collect())
    }

    /// Stop and remove a sandbox.
    ///
    /// Under `--features krunvm` this stops the VM host process, which
    /// ends the microVM and releases everything it held.
    async fn remove_sandbox(&self, id: &str) -> Result<()> {
        #[cfg_attr(not(feature = "krunvm"), allow(unused_variables, unused_mut))]
        let mut state = self
            .sandboxes
            .write()
            .await
            .remove(id)
            .ok_or_else(|| BackendError::NotFound(id.to_string()))?;

        // Stop the VM host process first so the guest stops using the
        // network socket, then stop the smoltcp task behind it.
        #[cfg(feature = "krunvm")]
        if let Some(mut shim) = state.shim.take() {
            shim.stop().await;
        }

        #[cfg(feature = "krunvm")]
        if let Some(mut smoltcp) = state.smoltcp.take()
            && let Err(e) = smoltcp.detach().await
        {
            tracing::warn!(sandbox_id = %id, "smoltcp teardown error: {e}");
        }

        // Drop the sandbox's snapshots too. Snapshots are bound to their
        // parent sandbox's lifetime; the proto keys list_snapshots by
        // sandbox_id, so a snapshot of a removed sandbox would be a
        // dangling reference no caller could ever reach. Reap their on-disk
        // archives as well so removal doesn't leak storage.
        let reaped: Vec<String> = {
            let mut snaps = self.snapshots.write().await;
            let reaped = snaps
                .iter()
                .filter(|(_, s)| s.sandbox_id == id)
                .map(|(k, _)| k.clone())
                .collect();
            snaps.retain(|_, snap| snap.sandbox_id != id);
            reaped
        };
        for snapshot_id in reaped {
            let _ = std::fs::remove_dir_all(self.snapshot_dir(&snapshot_id));
        }

        // Best-effort host cleanup: the agent socket and the unpacked rootfs.
        let _ = std::fs::remove_file(self.agent_socket_path(id));
        let sandbox_dir = self.data_dir.join("sandboxes").join(id);
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(sandbox_dir)).await;

        Ok(())
    }

    /// Count of active sandboxes.
    async fn count(&self) -> Result<usize> {
        Ok(self.sandboxes.read().await.len())
    }

    /// Signal a process to terminate by sending a Kill over its agent
    /// connection. In stub builds the process map is empty, so this is a
    /// no-op (the manager tears the process down by dropping its channels).
    ///
    /// SEC-017/018: verifies that `sandbox_id` matches the recorded
    /// owner of `pid` before signalling. A mismatched call returns
    /// Ok(()) (treat as not-found, don't leak which sandbox owns the
    /// pid per ADR-004) and logs at `debug!` so misuse is visible to
    /// operators who turn the level up; using `warn!` would surface
    /// the (caller, pid) pair on default-level shipping, which itself
    /// is the cross-tenant correlation ADR-004 wants suppressed.
    async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()> {
        let mut processes = self.processes.write().await;
        // Single match over the borrowed entry: the prior `get` + `remove`
        // pair had a dead-code defensive branch (the second `Some` was
        // unreachable under the held write lock) that misled readers
        // into thinking there was a real `None` case to handle.
        match processes.get(pid) {
            Some(record) if record.sandbox_id == sandbox_id => {
                // Owner matches; safe to remove + signal. `unwrap` is sound
                // because we just observed the entry under the write lock,
                // which we have held continuously.
                let record = processes
                    .remove(pid)
                    .expect("entry observed under write lock");
                // Best-effort send: a closed receiver just means the
                // process already exited and its bridge task is gone.
                let _ = record.kill_tx.send(()).await;
            }
            Some(_) => {
                tracing::debug!(
                    target: "ward::security_audit",
                    caller_sandbox = %sandbox_id,
                    pid = %pid,
                    "kill_process: caller sandbox does not own pid; refusing without signalling"
                );
            }
            None => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Snapshots
    //
    // In stub mode these methods track metadata only. The real backend will
    // call libkrun's checkpoint/restore APIs alongside the same bookkeeping;
    // the public signatures and the NotFound/Internal error contracts stay
    // unchanged so the manager and gRPC layers never need to be revisited.
    // -----------------------------------------------------------------------

    /// Take a disk-level snapshot of a sandbox: archive its rootfs and
    /// record metadata. This captures filesystem state only — libkrun 1.18
    /// exposes no live checkpoint API, so in-memory/CPU state is not saved
    /// (see ADR-009).
    async fn create_snapshot(&self, sandbox_id: &str, label: &str) -> Result<SnapshotInfo> {
        // Sandbox must exist — snapshotting a non-existent sandbox is a
        // user error, not an internal failure.
        if !self.sandboxes.read().await.contains_key(sandbox_id) {
            return Err(BackendError::NotFound(sandbox_id.to_string()));
        }

        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let rootfs = self.sandbox_rootfs(sandbox_id);
        let snap_dir = self.snapshot_dir(&snapshot_id);
        let archive = snap_dir.join("rootfs.tar");

        // Archive off the runtime to avoid blocking the reactor on large
        // rootfs trees.
        let size_bytes = {
            let rootfs = rootfs.clone();
            let archive = archive.clone();
            tokio::task::spawn_blocking(move || archive_rootfs(&rootfs, &archive))
                .await
                .map_err(|e| BackendError::Internal(format!("snapshot task: {e}")))??
        };

        let info = SnapshotInfo {
            snapshot_id: snapshot_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            label: label.to_string(),
            created_at: std::time::SystemTime::now(),
            size_bytes,
        };
        write_snapshot_metadata(&snap_dir.join("metadata.json"), &info)?;

        self.snapshots
            .write()
            .await
            .insert(snapshot_id, info.clone());
        Ok(info)
    }

    /// Restore a sandbox from a previously-taken snapshot by swapping its
    /// rootfs back to the archived contents. Verifies the snapshot exists
    /// AND belongs to the named sandbox first. Under `--features krunvm` the
    /// VM is also rebooted into the restored rootfs (gated; the filesystem
    /// swap itself is host-side and verified).
    async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()> {
        // Validate ownership, then release the lock before doing IO.
        {
            let guard = self.snapshots.read().await;
            let snap = guard
                .get(snapshot_id)
                .ok_or_else(|| BackendError::NotFound(snapshot_id.to_string()))?;
            if snap.sandbox_id != sandbox_id {
                // Cross-sandbox restore would let one sandbox roll into
                // another's state — exactly the kind of tenant boundary
                // we guard everywhere else. Surface as NotFound to avoid
                // leaking the snapshot's existence to the wrong caller.
                return Err(BackendError::NotFound(snapshot_id.to_string()));
            }
        }

        let archive = self.snapshot_dir(snapshot_id).join("rootfs.tar");
        let rootfs = self.sandbox_rootfs(sandbox_id);
        tokio::task::spawn_blocking(move || extract_rootfs(&archive, &rootfs))
            .await
            .map_err(|e| BackendError::Internal(format!("restore task: {e}")))??;

        // TODO(krunvm): reboot the VM into the restored rootfs. The
        // filesystem swap above is the host-side, verifiable half.
        Ok(())
    }

    /// List all snapshots taken from a given sandbox. Returns an empty
    /// vec for unknown sandboxes — list operations are intentionally
    /// lenient because callers commonly call list on missing entities
    /// to check existence.
    async fn list_snapshots(&self, sandbox_id: &str) -> Result<Vec<SnapshotInfo>> {
        let guard = self.snapshots.read().await;
        let mut out: Vec<SnapshotInfo> = guard
            .values()
            .filter(|s| s.sandbox_id == sandbox_id)
            .cloned()
            .collect();
        // Stable order: oldest first. HashMap iteration is unspecified.
        out.sort_by_key(|s| s.created_at);
        Ok(out)
    }

    async fn snapshot_owner(&self, snapshot_id: &str) -> Result<String> {
        let guard = self.snapshots.read().await;
        guard
            .get(snapshot_id)
            .map(|snap| snap.sandbox_id.clone())
            .ok_or_else(|| BackendError::NotFound(snapshot_id.to_string()))
    }

    /// Exec a command inside a running sandbox.
    async fn exec(
        &self,
        sandbox_id: &str,
        command: Vec<String>,
        _working_dir: Option<String>,
        _env: HashMap<String, String>,
    ) -> Result<ProcessHandle> {
        let _state = {
            let guard = self.sandboxes.read().await;
            guard
                .get(sandbox_id)
                .ok_or_else(|| BackendError::NotFound(sandbox_id.to_string()))?
                .info
                .clone()
        };

        let pid = uuid::Uuid::new_v4().to_string();

        // Real path: connect to the sandbox's guest agent over its host-side
        // Unix socket and bridge the connection to ProcessHandle channels.
        // Compile-verified under --features krunvm; running it needs a booted
        // microVM with the agent listening (see the spec's verification
        // ceiling).
        #[cfg(feature = "krunvm")]
        let handle = {
            let sock = self.agent_socket_path(sandbox_id);
            let stream = connect_agent_socket_with_timeout(
                tokio::net::UnixStream::connect(&sock),
                &sock,
                AGENT_CONNECT_TIMEOUT,
            )
            .await?;
            let (handle, kill_tx) = super::agent::drive_exec(
                stream,
                pid,
                sandbox_id.to_string(),
                command,
                _working_dir,
                _env,
            )
            .await
            .map_err(|e| BackendError::Exec(format!("guest agent exec: {e}")))?;
            // SEC-017/018: record (sandbox_id, kill_tx) so kill_process
            // can verify ownership at signal time.
            self.processes.write().await.insert(
                handle.pid.clone(),
                ProcessRecord {
                    sandbox_id: sandbox_id.to_string(),
                    kill_tx,
                },
            );
            handle
        };

        // Stub path: a tiny scripted output stream so StreamOutput has
        // something to deliver end-to-end without a real VM. Tests assert on
        // this shape.
        #[cfg(not(feature = "krunvm"))]
        let handle = {
            use crate::protocol::{StreamEvent, StreamEventKind};
            let (output_tx, output_rx) = tokio::sync::mpsc::channel::<StreamEvent>(16);
            let cmd_for_log = command.first().cloned().unwrap_or_default();
            tokio::spawn(async move {
                let started = std::time::SystemTime::now();
                let _ = output_tx
                    .send(StreamEvent {
                        kind: StreamEventKind::Stdout,
                        line: format!("stub: {cmd_for_log}"),
                        exit_code: None,
                        timestamp: std::time::SystemTime::now(),
                        duration_ms: 0,
                    })
                    .await;
                tokio::time::sleep(STUB_EXIT_DELAY).await;
                let _ = output_tx
                    .send(StreamEvent {
                        kind: StreamEventKind::Exit,
                        line: String::new(),
                        exit_code: Some(0),
                        timestamp: std::time::SystemTime::now(),
                        duration_ms: started.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0),
                    })
                    .await;
                // output_tx drops here → channel closes → consumer sees None.
            });

            // Stdin half: the drain task keeps stdin_rx alive so writes don't
            // fail with "channel closed"; bytes are discarded.
            let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
            tokio::spawn(async move {
                while let Some(_chunk) = stdin_rx.recv().await {
                    // discard
                }
            });

            ProcessHandle {
                pid,
                sandbox_id: sandbox_id.to_string(),
                stdin_tx: Some(stdin_tx),
                output_rx: Some(output_rx),
            }
        };

        Ok(handle)
    }
}

/// Number of sandbox-id characters used in an agent socket file name.
const SOCKET_ID_LEN: usize = 16;

/// Permissions of the per-user agent socket directory (rwx------).
#[cfg(feature = "krunvm")]
const SOCKET_DIR_MODE: u32 = 0o700;

/// Per-user directory holding agent sockets. Kept short on purpose, see
/// [`KrunvmBackend::agent_socket_path`].
fn agent_socket_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp").join(format!("ward-{}", rustix::process::getuid().as_raw()))
}

/// Create the agent socket directory, refusing one that another user owns
/// or that is a symlink.
#[cfg(feature = "krunvm")]
fn ensure_agent_socket_dir() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let dir = agent_socket_dir();
    match std::fs::DirBuilder::new()
        .mode(SOCKET_DIR_MODE)
        .create(&dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(BackendError::Io(e)),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(BackendError::Io)?;
    if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() {
        return Err(BackendError::Internal(format!(
            "{} must be a directory owned by the current user",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Write the embedded guest agent to `/ward-agent` in a sandbox rootfs.
///
/// The old file is removed first because the rootfs is hard-linked from the
/// shared image cache. Writing through a link would change the cached image.
#[cfg(feature = "krunvm")]
async fn install_guest_agent(rootfs: &std::path::Path) -> Result<()> {
    let dest = rootfs.join(AGENT_GUEST_PATH.trim_start_matches('/'));
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let agent = ward_bundle::guest_agent()
            .map_err(|e| BackendError::Internal(format!("embedded guest agent: {e}")))?;
        match std::fs::remove_file(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(BackendError::Io(e)),
        }
        std::fs::write(&dest, agent).map_err(BackendError::Io)?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(AGENT_FILE_MODE))
            .map_err(BackendError::Io)
    })
    .await
    .map_err(|e| BackendError::Internal(format!("install agent task: {e}")))?
}

impl KrunvmBackend {
    /// Host-side Unix socket path bridging to the sandbox's agent vsock.
    ///
    /// Lives in a short per-user directory because `sockaddr_un` paths are
    /// limited to 104 bytes on macOS, which a long data dir would exceed.
    fn agent_socket_path(&self, sandbox_id: &str) -> std::path::PathBuf {
        let short_id: String = sandbox_id
            .chars()
            .filter(|c| *c != '-')
            .take(SOCKET_ID_LEN)
            .collect();
        agent_socket_dir().join(format!("{short_id}.sock"))
    }

    /// Unpacked rootfs directory for a sandbox.
    fn sandbox_rootfs(&self, sandbox_id: &str) -> std::path::PathBuf {
        self.data_dir
            .join("sandboxes")
            .join(sandbox_id)
            .join("rootfs")
    }

    /// On-disk directory holding a snapshot's archive and metadata.
    fn snapshot_dir(&self, snapshot_id: &str) -> std::path::PathBuf {
        self.data_dir.join("snapshots").join(snapshot_id)
    }

    /// Start the VM host process for a sandbox and wait until its guest
    /// agent answers.
    #[cfg(feature = "krunvm")]
    async fn boot_vm(
        &self,
        id: &str,
        opts: &CreateOpts,
        rootfs: &std::path::Path,
    ) -> Result<BootedVm> {
        use super::vmshim::{MountSpec, NetSpec, ShimProcess, VmSpec};
        use std::os::fd::{FromRawFd, OwnedFd};

        install_guest_agent(rootfs).await?;
        ensure_agent_socket_dir()?;
        let agent_socket = self.agent_socket_path(id);
        let _ = std::fs::remove_file(&agent_socket);

        let (smoltcp, egress, net_fd) = if opts.egress.mode != EgressMode::Deny
            && self.network_backend == NetworkBackendChoice::Smoltcp
        {
            // The proxy is the single source of truth for this sandbox's
            // allow/deny decisions; the spawned Stack task consults it
            // through the closure below on every flow.
            let egress_proxy = Arc::new(EgressProxy::new(id.to_string(), opts.egress.clone()));
            let egress_for_check = Arc::clone(&egress_proxy);
            let egress_check: Box<ward_net::smoltcp_backend::EgressCheckFn> =
                Box::new(move |domain: &str, port: u16| {
                    let proxy = Arc::clone(&egress_for_check);
                    let domain = domain.to_string();
                    Box::pin(async move { proxy.check(&domain, port).await })
                });
            let mut handle = ward_net::smoltcp_backend::spawn_for_sandbox(
                id,
                &ward_net::AttachOptions::default(),
                egress_check,
            )
            .await
            .map_err(|e| BackendError::Internal(format!("smoltcp spawn: {e}")))?;
            let raw = std::mem::replace(&mut handle.guest_fd, -1);
            // SAFETY: `raw` is the open guest end of the socketpair and the
            // handle no longer refers to it.
            let owned = unsafe { OwnedFd::from_raw_fd(raw) };
            (Some(handle), Some(egress_proxy), Some((raw, owned)))
        } else {
            // Deny mode or NetworkBackendChoice::None: no datapath to spawn.
            (None, None, None)
        };

        // Bind mounts become virtiofs shares. Volume attach (ext4 images as
        // block devices) is not wired yet; volume_ids are validated upstream.
        let mounts = opts
            .mounts
            .iter()
            .enumerate()
            .map(|(idx, m)| MountSpec {
                tag: format!("ward-mnt-{idx}"),
                path: std::path::PathBuf::from(&m.source),
                read_only: m.readonly,
            })
            .collect();
        let _ = &opts.volume_ids;

        let (net, inherit) = match net_fd {
            Some((raw, owned)) => (
                Some(NetSpec {
                    fd: raw,
                    mac: SMOLTCP_GUEST_MAC,
                }),
                Some(owned),
            ),
            None => (None, None),
        };
        let spec = VmSpec {
            rootfs: rootfs.to_path_buf(),
            cpus: opts.resources.cpus,
            memory_mb: opts.resources.memory_mb,
            exec_path: AGENT_GUEST_PATH.to_string(),
            agent_port: AGENT_VSOCK_PORT,
            agent_socket: agent_socket.clone(),
            mounts,
            net,
        };

        let mut shim = ShimProcess::spawn(&spec, inherit)
            .await
            .map_err(|e| BackendError::Internal(format!("start VM host process: {e}")))?;
        if let Err(e) = wait_for_agent(&mut shim, &agent_socket).await {
            shim.stop().await;
            if let Some(mut handle) = smoltcp {
                let _ = handle.detach().await;
            }
            let _ = std::fs::remove_file(&agent_socket);
            return Err(e);
        }
        Ok(BootedVm {
            shim,
            smoltcp,
            egress,
        })
    }
}

/// A started microVM and the host-side pieces that serve it.
#[cfg(feature = "krunvm")]
struct BootedVm {
    shim: super::vmshim::ShimProcess,
    smoltcp: Option<ward_net::smoltcp_backend::SmoltcpHandle>,
    egress: Option<Arc<EgressProxy>>,
}

/// How long a new VM gets to start its guest agent.
#[cfg(feature = "krunvm")]
const AGENT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Pause between readiness probes.
#[cfg(feature = "krunvm")]
const AGENT_READY_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Per-probe limit for the connect and the `Ping` round trip.
#[cfg(feature = "krunvm")]
const AGENT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Probe the guest agent until it answers `Ping`, the VM host process dies
/// or [`AGENT_READY_TIMEOUT`] passes.
#[cfg(feature = "krunvm")]
async fn wait_for_agent(
    shim: &mut super::vmshim::ShimProcess,
    socket: &std::path::Path,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + AGENT_READY_TIMEOUT;
    loop {
        if !shim.is_running() {
            return Err(BackendError::Internal(
                "VM host process exited before the guest agent was ready".to_string(),
            ));
        }
        let probe = async {
            let stream = tokio::net::UnixStream::connect(socket).await?;
            super::agent::ping(stream).await
        };
        if matches!(
            tokio::time::timeout(AGENT_PROBE_TIMEOUT, probe).await,
            Ok(Ok(()))
        ) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BackendError::Internal(format!(
                "guest agent did not become ready within {}s",
                AGENT_READY_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(AGENT_READY_POLL).await;
    }
}

// ---------------------------------------------------------------------------
// Snapshot archive helpers (host-side, hypervisor-independent)
// ---------------------------------------------------------------------------

/// Archive a sandbox rootfs into a tar file, returning the archive size in
/// bytes. A missing rootfs yields a valid (empty) archive rather than an
/// error, so snapshotting a not-yet-populated sandbox still succeeds.
fn archive_rootfs(rootfs: &std::path::Path, archive: &std::path::Path) -> Result<u64> {
    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent).map_err(BackendError::Io)?;
    }
    // SEC-004: snapshot tar contains the full guest rootfs (including any
    // secrets the guest wrote to disk). Force 0600 so other local users
    // on multi-user hosts cannot read another tenant's snapshots.
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(archive).map_err(BackendError::Io)?;
    let mut builder = tar::Builder::new(file);
    if rootfs.is_dir() {
        builder
            .append_dir_all(".", rootfs)
            .map_err(|e| BackendError::Internal(format!("archive rootfs: {e}")))?;
    }
    builder
        .finish()
        .map_err(|e| BackendError::Internal(format!("finish archive: {e}")))?;
    drop(builder);
    Ok(std::fs::metadata(archive).map_err(BackendError::Io)?.len())
}

/// Replace `dest` with the contents of a rootfs archive.
///
/// SEC-001: bare `tar::Archive::unpack` follows on-disk symlinks and can
/// write entries outside `dest` (e.g. an archive containing a `etc ->
/// /etc` symlink followed by files under `etc/cron.d/`). We enumerate
/// entries explicitly, reject absolute paths, parent-traversal, and
/// unsafe symlink/hardlink targets, then use `entry.unpack_in(dest)`
/// (which re-validates each path against the destination root).
///
/// SEC-014: stage into a sibling temp dir and atomically rename on
/// success so a partial extract does not leave `dest` half-populated.
fn extract_rootfs(archive: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    if !archive.exists() {
        return Err(BackendError::NotFound(format!(
            "snapshot archive missing: {}",
            archive.display()
        )));
    }
    let parent = dest.parent().ok_or_else(|| {
        BackendError::Internal(format!("restore: no parent for {}", dest.display()))
    })?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&staging).map_err(BackendError::Io)?;

    let file = std::fs::File::open(archive).map_err(BackendError::Io)?;
    let mut ar = tar::Archive::new(file);
    // set_overwrite(true): tar crate replaces existing destination files
    // instead of erroring (safe here because we extract into a fresh
    // staging dir). set_preserve_permissions(false): drop setuid/setgid
    // bits from tar headers. Combined with per-entry path validation
    // below and `unpack_in` (which enforces destination containment),
    // the extract cannot write outside `staging`.
    ar.set_overwrite(true);
    ar.set_preserve_permissions(false);

    // Wrap the per-entry loop so we can clean up `staging` on ANY error
    // path (including unpack_in failures) without repeating the call
    // site at every `?`.
    let extract_result = (|| -> Result<()> {
        for entry in ar
            .entries()
            .map_err(|e| BackendError::Internal(format!("read archive: {e}")))?
        {
            let mut entry =
                entry.map_err(|e| BackendError::Internal(format!("read entry: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| BackendError::Internal(format!("entry path: {e}")))?
                .into_owned();
            // Reject absolute paths and any traversal component.
            if path.is_absolute()
                || path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::RootDir
                    )
                })
            {
                return Err(BackendError::Internal(format!(
                    "snapshot archive contains unsafe path: {}",
                    path.display()
                )));
            }
            // Reject symlink/hardlink entries pointing outside the dest.
            if matches!(
                entry.header().entry_type(),
                tar::EntryType::Symlink | tar::EntryType::Link
            ) {
                let link = entry
                    .link_name()
                    .map_err(|e| BackendError::Internal(format!("link target: {e}")))?
                    .ok_or_else(|| BackendError::Internal("link entry missing target".into()))?
                    .into_owned();
                if link.is_absolute()
                    || link
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(BackendError::Internal(format!(
                        "snapshot archive contains unsafe link target: {}",
                        link.display()
                    )));
                }
            }
            entry.unpack_in(&staging).map_err(|e| {
                BackendError::Internal(format!("extract entry {}: {e}", path.display()))
            })?;
        }
        Ok(())
    })();
    if let Err(e) = extract_result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // SEC-014: atomic swap. Each supported platform uses its native
    // atomic-exchange syscall so a crash between unlink and rename can
    // never leave `dest` absent. EXCHANGE / SWAP require both paths to
    // exist; first-time restore (no dest yet) falls back to a plain
    // rename. Linux: rustix renameat_with(RENAME_EXCHANGE). macOS:
    // renamex_np(RENAME_SWAP), declared inline above. Other Unix
    // targets keep the two-step residual; ward does not support any.
    //
    // The `dest.exists()` / `atomic_swap()` pair looks like a TOCTOU
    // window, but the snapshot directory is owned exclusively by
    // ward-core: no other process (or, with the daemon's serialised
    // per-sandbox state, even another thread within the daemon)
    // races us between these two calls. A concurrent restore for the
    // same sandbox is impossible by the SandboxManager's contract,
    // and an external attacker who could create `dest` between the
    // check and the swap would already have arbitrary write access
    // to the daemon's data directory.
    if dest.exists() {
        atomic_swap(&staging, dest)?;
        let _ = std::fs::remove_dir_all(&staging);
    } else {
        std::fs::rename(&staging, dest).map_err(BackendError::Io)?;
    }
    Ok(())
}

/// Atomically swap two existing paths: after the call, `dest` holds
/// what was in `staging` and `staging` holds what was in `dest`. Both
/// paths must exist. Used by `extract_rootfs` (SEC-014).
fn atomic_swap(staging: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        renameat_with(CWD, staging, CWD, dest, RenameFlags::EXCHANGE).map_err(
            |e: rustix::io::Errno| BackendError::Internal(format!("renameat2 EXCHANGE: {e}")),
        )
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(staging.as_os_str().as_bytes())
            .map_err(|e| BackendError::Internal(format!("staging path contains NUL: {e}")))?;
        let to_c = std::ffi::CString::new(dest.as_os_str().as_bytes())
            .map_err(|e| BackendError::Internal(format!("dest path contains NUL: {e}")))?;
        // SAFETY: the four documented invariants for this `unsafe` call:
        //   1. `from_c` and `to_c` each point to a NUL-terminated C
        //      string we own (via `CString`), kept alive for the
        //      duration of the call (the `CString`s are bound to
        //      named locals in this scope).
        //   2. `RENAME_SWAP` (= 0x2) is a documented flag from
        //      Darwin's `<sys/stdio.h>`; passing it is the contract
        //      this call exists for.
        //   3. `renamex_np` does not require `from` and `to` to be
        //      non-aliased; both being the same path would simply
        //      no-op (and would never happen in this codebase).
        //   4. Errno is read via `std::io::Error::last_os_error()`
        //      immediately after the -1 return, before any other
        //      libc call could clobber it on this thread.
        let rc = unsafe { renamex_np(from_c.as_ptr(), to_c.as_ptr(), RENAME_SWAP) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(BackendError::Internal(format!("renamex_np SWAP: {err}")));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Non-Linux, non-macOS Unix: no atomic-swap syscall available
        // in this codebase; fall back to the two-step residual. Ward
        // doesn't support any of these targets today, so this branch
        // exists only to keep the function total.
        std::fs::remove_dir_all(dest).map_err(BackendError::Io)?;
        std::fs::rename(staging, dest).map_err(BackendError::Io)
    }
}

/// Persist snapshot metadata as JSON alongside its archive.
fn write_snapshot_metadata(path: &std::path::Path, info: &SnapshotInfo) -> Result<()> {
    let created = info
        .created_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let json = serde_json::json!({
        "snapshot_id": info.snapshot_id,
        "sandbox_id": info.sandbox_id,
        "label": info.label,
        "created_at_unix": created,
        "size_bytes": info.size_bytes,
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(BackendError::Io)?;
    }
    let bytes = serde_json::to_vec_pretty(&json)
        .map_err(|e| BackendError::Internal(format!("serialise snapshot metadata: {e}")))?;
    std::fs::write(path, bytes).map_err(BackendError::Io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: snapshot behaviour
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::image::{ImagePuller, ImageStore};
    use crate::protocol::{CommunicationPolicy, CreateOpts, EgressPolicy};
    use pretty_assertions::assert_eq;
    use std::path::Path;

    /// Offline image puller for tests: creates a minimal rootfs without
    /// touching the network. Returns a digest derived from the reference
    /// so distinct image refs get distinct cache entries.
    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl ImagePuller for FakePuller {
        async fn pull(&self, reference: &str, dest: &Path) -> crate::backend::Result<String> {
            std::fs::create_dir_all(dest.join("bin")).map_err(crate::backend::BackendError::Io)?;
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(format!("sha256:{hash:016x}"))
        }
    }

    /// Build a fresh backend rooted in a tempdir. Injects a FakePuller so
    /// `create_sandbox` populates the sandbox rootfs without network access.
    /// Leaks the TempDir intentionally so the data_dir survives the lifetime
    /// of the returned backend across async boundaries.
    fn backend_in_tempdir() -> KrunvmBackend {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        std::mem::forget(tmp);
        KrunvmBackend::with_image_store_for_test(path, store)
    }

    /// Same as `backend_in_tempdir`, but lets a test pick which network
    /// backend `create_sandbox` wires up instead of the default.
    #[cfg(feature = "krunvm")]
    fn backend_in_tempdir_with_network(network_backend: NetworkBackendChoice) -> KrunvmBackend {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        std::mem::forget(tmp);
        KrunvmBackend::new_internal(path, network_backend, store)
    }

    fn create_opts() -> CreateOpts {
        CreateOpts {
            image: "alpine:latest".into(),
            mounts: vec![],
            volume_ids: vec![],
            egress: EgressPolicy::default(),
            resources: crate::protocol::ResourceLimits::default(),
            env: HashMap::new(),
            from_snapshot: None,
            comms: CommunicationPolicy::default(),
        }
    }

    async fn create_sandbox(backend: &KrunvmBackend, id: &str) {
        backend
            .create_sandbox(id.to_string(), &create_opts())
            .await
            .expect("create_sandbox");
    }

    // ----- create_snapshot ----------------------------------------------

    #[tokio::test]
    async fn given_existing_sandbox_when_create_snapshot_then_returns_info_with_new_uuid() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;

        // Act
        let snap = backend
            .create_snapshot("sb1", "checkpoint-1")
            .await
            .expect("create_snapshot");

        // Assert: fresh UUID, label round-trips, sandbox_id matches.
        assert_eq!(snap.snapshot_id.len(), 36);
        assert_eq!(snap.sandbox_id, "sb1");
        assert_eq!(snap.label, "checkpoint-1");
        // A real (even empty) tar archive is materialised, so size is > 0.
        assert!(snap.size_bytes > 0, "expected a real archive size");
    }

    // ----- disk-level snapshot: archive / restore / from_snapshot --------

    /// Write `name`→`contents` into a sandbox's rootfs (creating it).
    fn seed_rootfs(backend: &KrunvmBackend, sandbox_id: &str, name: &str, contents: &[u8]) {
        let rootfs = backend.sandbox_rootfs(sandbox_id);
        std::fs::create_dir_all(&rootfs).expect("mk rootfs");
        std::fs::write(rootfs.join(name), contents).expect("write file");
    }

    #[tokio::test]
    async fn given_rootfs_content_when_create_snapshot_then_archive_written() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        seed_rootfs(&backend, "sb1", "hello.txt", b"snapshot me");

        // Act
        let snap = backend
            .create_snapshot("sb1", "c1")
            .await
            .expect("snapshot");

        // Assert: the archive exists on disk and carries the file's bytes.
        let archive = backend.snapshot_dir(&snap.snapshot_id).join("rootfs.tar");
        assert!(archive.exists(), "archive should be materialised");
        assert!(snap.size_bytes > 1024, "archive should exceed an empty tar");
    }

    #[tokio::test]
    async fn given_snapshot_when_restore_then_rootfs_content_restored() {
        // Arrange: snapshot a file, then mutate the rootfs.
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        seed_rootfs(&backend, "sb1", "data.txt", b"original");
        let snap = backend
            .create_snapshot("sb1", "c1")
            .await
            .expect("snapshot");
        std::fs::write(backend.sandbox_rootfs("sb1").join("data.txt"), b"changed").unwrap();

        // Act
        backend
            .restore_snapshot("sb1", &snap.snapshot_id)
            .await
            .expect("restore");

        // Assert: the file is back to its snapshotted contents.
        let restored = std::fs::read(backend.sandbox_rootfs("sb1").join("data.txt")).unwrap();
        assert_eq!(restored, b"original");
    }

    #[tokio::test]
    async fn given_from_snapshot_when_create_then_new_rootfs_seeded() {
        // Arrange: snapshot sb1's rootfs.
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        seed_rootfs(&backend, "sb1", "seed.txt", b"from-snapshot");
        let snap = backend
            .create_snapshot("sb1", "c1")
            .await
            .expect("snapshot");

        // Act: create sb2 from that snapshot.
        let opts = CreateOpts {
            from_snapshot: Some(snap.snapshot_id.clone()),
            ..create_opts()
        };
        backend
            .create_sandbox("sb2".into(), &opts)
            .await
            .expect("create from snapshot");

        // Assert: sb2's rootfs was seeded with the snapshot contents.
        let seeded = std::fs::read(backend.sandbox_rootfs("sb2").join("seed.txt")).unwrap();
        assert_eq!(seeded, b"from-snapshot");
    }

    #[tokio::test]
    async fn given_unknown_from_snapshot_when_create_then_not_found() {
        let backend = backend_in_tempdir();
        let opts = CreateOpts {
            from_snapshot: Some("nonexistent".into()),
            ..create_opts()
        };
        let err = backend
            .create_sandbox("sb2".into(), &opts)
            .await
            .expect_err("unknown snapshot");
        assert!(matches!(err, BackendError::NotFound(_)));
    }

    #[tokio::test]
    async fn given_sandbox_with_snapshot_when_removed_then_archive_dir_deleted() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        seed_rootfs(&backend, "sb1", "f", b"x");
        let snap = backend
            .create_snapshot("sb1", "c1")
            .await
            .expect("snapshot");
        let snap_dir = backend.snapshot_dir(&snap.snapshot_id);
        assert!(snap_dir.exists());

        // Act
        backend.remove_sandbox("sb1").await.expect("remove");

        // Assert: the on-disk archive directory is reaped with the sandbox.
        assert!(!snap_dir.exists(), "snapshot dir should be deleted");
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_create_snapshot_then_not_found() {
        // Arrange
        let backend = backend_in_tempdir();

        // Act
        let err = backend
            .create_snapshot("ghost", "x")
            .await
            .expect_err("unknown sandbox");

        // Assert
        assert!(matches!(err, BackendError::NotFound(_)));
    }

    #[tokio::test]
    async fn given_multiple_create_snapshot_calls_when_completed_then_each_has_unique_id() {
        // Arrange: two snapshots of the same sandbox must get distinct
        // ids — otherwise restore would be ambiguous.
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;

        // Act
        let a = backend.create_snapshot("sb1", "first").await.unwrap();
        let b = backend.create_snapshot("sb1", "second").await.unwrap();

        // Assert
        assert_ne!(a.snapshot_id, b.snapshot_id);
    }

    // ----- restore_snapshot ---------------------------------------------

    #[tokio::test]
    async fn given_known_snapshot_when_restore_then_succeeds() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        let snap = backend.create_snapshot("sb1", "x").await.unwrap();

        // Act
        let result = backend.restore_snapshot("sb1", &snap.snapshot_id).await;

        // Assert: stub returns Ok; the real backend would also rewind
        // the VM state.
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn given_unknown_snapshot_when_restore_then_not_found() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;

        // Act
        let err = backend
            .restore_snapshot("sb1", "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown snapshot");

        // Assert
        assert!(matches!(err, BackendError::NotFound(_)));
    }

    #[tokio::test]
    async fn given_snapshot_of_other_sandbox_when_restore_then_not_found() {
        // Arrange: tenant isolation regression — snapshot belongs to sb1,
        // restoring it as sb2 must fail as if the snapshot didn't exist
        // for sb2 (don't leak its existence across sandboxes).
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        create_sandbox(&backend, "sb2").await;
        let snap = backend.create_snapshot("sb1", "x").await.unwrap();

        // Act
        let err = backend
            .restore_snapshot("sb2", &snap.snapshot_id)
            .await
            .expect_err("cross-sandbox restore");

        // Assert
        assert!(matches!(err, BackendError::NotFound(_)));
    }

    // ----- list_snapshots -----------------------------------------------

    #[tokio::test]
    async fn given_no_snapshots_when_list_then_empty() {
        // Arrange
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;

        // Act
        let list = backend.list_snapshots("sb1").await.unwrap();

        // Assert
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn given_unknown_sandbox_when_list_snapshots_then_empty_not_error() {
        // Arrange: list operations are lenient on unknown ids — callers
        // commonly use list to check existence without an error.
        let backend = backend_in_tempdir();

        // Act
        let list = backend.list_snapshots("ghost").await.unwrap();

        // Assert
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn given_multiple_snapshots_when_list_then_returns_all_for_that_sandbox() {
        // Arrange: two sandboxes, three snapshots total (2 + 1). List
        // for sb1 returns only its two.
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        create_sandbox(&backend, "sb2").await;
        backend.create_snapshot("sb1", "a").await.unwrap();
        backend.create_snapshot("sb1", "b").await.unwrap();
        backend.create_snapshot("sb2", "c").await.unwrap();

        // Act
        let list = backend.list_snapshots("sb1").await.unwrap();

        // Assert: exactly two, both with sandbox_id "sb1".
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|s| s.sandbox_id == "sb1"));
    }

    // ----- lifecycle: remove_sandbox cleans up snapshots ----------------

    #[tokio::test]
    async fn given_sandbox_with_snapshots_when_removed_then_snapshots_gone() {
        // Arrange: regression guard — snapshots are bound to sandbox
        // lifetime. Removing the parent reaps its snapshots so they
        // don't become dangling rows in the broker's view.
        let backend = backend_in_tempdir();
        create_sandbox(&backend, "sb1").await;
        let snap = backend.create_snapshot("sb1", "x").await.unwrap();

        // Act
        backend.remove_sandbox("sb1").await.unwrap();

        // Assert: snapshot is gone, attempting to restore returns NotFound.
        create_sandbox(&backend, "sb1-again").await;
        let err = backend
            .restore_snapshot("sb1-again", &snap.snapshot_id)
            .await
            .expect_err("dangling snapshot");
        assert!(matches!(err, BackendError::NotFound(_)));
    }

    // ----- SEC-017/018: kill_process ownership verification -------------

    #[tokio::test]
    async fn given_pid_owned_by_sandbox_a_when_kill_with_sandbox_b_then_refused_silently() {
        // Arrange: insert a process record claiming sandbox A owns this
        // pid. (Stub-mode exec doesn't populate the map, so the test
        // bypasses it and writes directly — that's the point: we're
        // testing the Backend-trait-level guard, not the manager.)
        let backend = backend_in_tempdir();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        backend.processes.write().await.insert(
            "pid-1".to_string(),
            ProcessRecord {
                sandbox_id: "sandbox-A".to_string(),
                kill_tx: tx,
            },
        );

        // Act: a caller claiming sandbox-B tries to kill pid-1.
        backend
            .kill_process("sandbox-B", "pid-1")
            .await
            .expect("kill_process should return Ok for the mismatch case");

        // Assert: no kill signal was sent (the receiver should NOT have
        // a message available immediately) AND the process record is
        // still in the map (was not removed).
        assert!(
            rx.try_recv().is_err(),
            "cross-sandbox kill leaked a signal — ownership check is broken"
        );
        assert!(
            backend.processes.read().await.contains_key("pid-1"),
            "cross-sandbox kill removed the process record — ownership check is broken"
        );
    }

    #[tokio::test]
    async fn given_pid_owned_by_sandbox_a_when_kill_with_sandbox_a_then_signal_sent_and_removed() {
        // Arrange
        let backend = backend_in_tempdir();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        backend.processes.write().await.insert(
            "pid-1".to_string(),
            ProcessRecord {
                sandbox_id: "sandbox-A".to_string(),
                kill_tx: tx,
            },
        );

        // Act: same-sandbox kill — should succeed.
        backend
            .kill_process("sandbox-A", "pid-1")
            .await
            .expect("kill_process should succeed for owner");

        // Assert: signal received + record gone.
        assert!(
            rx.recv().await.is_some(),
            "kill signal was not sent to the owner"
        );
        assert!(
            !backend.processes.read().await.contains_key("pid-1"),
            "process record was not removed after successful kill"
        );
    }

    #[tokio::test]
    async fn given_unknown_pid_when_kill_then_ok_noop() {
        // Arrange: empty processes map (the common case in stub mode).
        let backend = backend_in_tempdir();

        // Act + Assert: kill returns Ok even when pid is unknown —
        // matches the pre-SEC-017/018 contract that the manager relies
        // on for its lifecycle teardown path.
        backend
            .kill_process("sandbox-A", "pid-doesnt-exist")
            .await
            .expect("kill_process on unknown pid should be Ok no-op");
    }

    // ----- connect_agent_socket_with_timeout (audit fix) -------------------

    #[tokio::test]
    async fn given_listening_socket_when_connect_agent_socket_then_returns_stream() {
        // Arrange
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        // Act
        let result = connect_agent_socket_with_timeout(
            tokio::net::UnixStream::connect(&sock_path),
            &sock_path,
            AGENT_CONNECT_TIMEOUT,
        )
        .await;

        // Assert
        assert!(result.is_ok(), "expected a live agent socket to connect");
    }

    #[tokio::test]
    async fn given_connect_that_never_resolves_when_connect_agent_socket_then_times_out() {
        // Arrange: a connect future that never completes, standing in for a
        // wedged guest agent. connect_agent_socket_with_timeout's doc
        // comment explains why a Unix-domain connect can't be made to hang
        // deterministically at the OS level, so this exercises the timeout
        // wrapper directly instead.
        let sock_path = std::path::PathBuf::from("/nonexistent/agent.sock");
        let never_connects = std::future::pending::<std::io::Result<tokio::net::UnixStream>>();
        let timeout = std::time::Duration::from_millis(50);
        tokio::time::pause();

        // Act
        let fut = connect_agent_socket_with_timeout(never_connects, &sock_path, timeout);
        tokio::pin!(fut);
        tokio::time::advance(timeout + std::time::Duration::from_millis(1)).await;
        let result = fut.await;

        // Assert
        match result {
            Err(BackendError::Exec(msg)) => {
                assert!(
                    msg.contains("timed out"),
                    "expected a timeout message, got {msg:?}"
                );
            }
            other => panic!("expected BackendError::Exec on timeout, got {other:?}"),
        }
    }

    // ----- network backend selection: smoltcp -----------------------------

    #[cfg(feature = "krunvm")]
    #[tokio::test]
    async fn given_network_backend_smoltcp_when_create_sandbox_then_smoltcp_handle_set() {
        // Arrange: a backend configured for the smoltcp network backend,
        // with egress open so create_sandbox doesn't skip network setup
        // (EgressMode::Deny, the default, never wires up a backend).
        let backend = backend_in_tempdir_with_network(NetworkBackendChoice::Smoltcp);
        let opts = CreateOpts {
            egress: EgressPolicy {
                mode: EgressMode::Open,
                domains: vec![],
            },
            ..create_opts()
        };

        // Act
        backend
            .create_sandbox("sb1".to_string(), &opts)
            .await
            .expect("create_sandbox");

        // Assert: the sandbox's smoltcp handle was populated.
        let sandboxes = backend.sandboxes.read().await;
        let state = sandboxes.get("sb1").expect("sandbox state recorded");
        assert!(
            state.smoltcp.is_some(),
            "expected smoltcp handle to be set for NetworkBackendChoice::Smoltcp"
        );
    }
}
