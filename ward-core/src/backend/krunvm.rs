// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! MicroVM backend using libkrun.
//!
//! All `unsafe` calls to the libkrun C ABI are confined to this module.
//! FFI declarations live in `super::krun_ffi` (hand-maintained, no
//! `krun-sys` crate, no bindgen). The public API is fully safe Rust.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::{Backend, BackendError, ProcessHandle, Result};
use crate::protocol::{
    CreateOpts, EgressMode, ResourceLimits, SandboxInfo, SandboxStatus, SnapshotInfo,
};

/// vsock port the guest agent listens on; must match `ward-agent`.
/// Only referenced by the krunvm-gated FFI wrappers.
#[allow(dead_code)]
const AGENT_VSOCK_PORT: u32 = 1024;

/// Path of the agent binary inside every sandbox rootfs.
/// Only referenced by the krunvm-gated FFI wrappers.
#[allow(dead_code)]
const AGENT_GUEST_PATH: &str = "/ward-agent";

// ---------------------------------------------------------------------------
// libc bindings used only by the krunvm boot path
// ---------------------------------------------------------------------------
//
// We need `write(2)` to poke the shutdown eventfd that libkrun returns
// from `krun_get_shutdown_eventfd`. Declaring it directly here keeps
// us off the `libc` crate dependency, which is otherwise unjustified
// (one syscall, two-line declaration).

#[cfg(feature = "krunvm")]
unsafe extern "C" {
    fn write(fd: std::ffi::c_int, buf: *const std::ffi::c_void, count: usize) -> isize;
}

// ---------------------------------------------------------------------------
// Per-sandbox state
// ---------------------------------------------------------------------------

/// Resources tied to a running microVM. Only populated when the
/// `krunvm` feature is enabled and `krun_start_enter` has been spawned.
#[cfg(feature = "krunvm")]
#[derive(Debug)]
struct VmRuntime {
    /// OS thread running the blocking `krun_start_enter` call. Joined
    /// in `remove_sandbox` after the shutdown eventfd is poked. The
    /// return value is `krun_start_enter`'s exit code (negative on
    /// libkrun-side failure).
    thread: std::thread::JoinHandle<i32>,
    /// Eventfd handed out by `krun_get_shutdown_eventfd(ctx_id)`.
    /// Writing 8 bytes of `u64(1)` to it asks the VM to unwind. We do
    /// NOT close this fd ourselves: libkrun owns its lifetime and
    /// reaps it during `krun_free_ctx`. Stored as a raw `c_int` for
    /// that reason. Negative values are tolerated (some platforms
    /// signal "no fd available" with -1).
    shutdown_fd: std::os::fd::RawFd,
}

#[derive(Debug)]
struct SandboxState {
    info: SandboxInfo,
    /// krun context ID returned by krun_create_ctx().
    /// 0 means not yet started.
    ctx_id: u32,
    /// VM thread + shutdown eventfd. `None` in stub builds and during
    /// the brief window before `krun_start_enter` is spawned. Always
    /// `Some` in `--features krunvm` after `create_sandbox` returns Ok.
    #[cfg(feature = "krunvm")]
    vm: Option<VmRuntime>,
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
}

impl KrunvmBackend {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
            processes: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait::async_trait]
impl Backend for KrunvmBackend {
    /// Create a new sandbox and start the microVM.
    ///
    /// Under `--features krunvm` this configures the libkrun context,
    /// captures the shutdown eventfd, and spawns the dedicated OS
    /// thread that runs `krun_start_enter`. The thread runs for the
    /// lifetime of the VM and is reaped in `remove_sandbox`.
    async fn create_sandbox(&self, id: String, opts: &CreateOpts) -> Result<SandboxInfo> {
        let ctx_id = self.krun_create_ctx()?;
        self.krun_apply_resources(ctx_id, &opts.resources)?;

        let rootfs = self.sandbox_rootfs(&id);

        // Seed the rootfs from a snapshot if requested. The snapshot must
        // exist; its archived filesystem becomes this sandbox's starting
        // state (host-side; the VM then boots from it).
        if let Some(snapshot_id) = &opts.from_snapshot {
            if !self.snapshots.read().await.contains_key(snapshot_id) {
                return Err(BackendError::NotFound(snapshot_id.clone()));
            }
            let archive = self.snapshot_dir(snapshot_id).join("rootfs.tar");
            let dest = rootfs.clone();
            tokio::task::spawn_blocking(move || extract_rootfs(&archive, &dest))
                .await
                .map_err(|e| BackendError::Internal(format!("from_snapshot task: {e}")))??;
        }

        self.krun_set_root(ctx_id, &rootfs)?;

        if opts.egress.mode != EgressMode::Deny {
            // TODO: configure virtio-net and attach egress proxy TAP.
        }

        // Bind mounts → virtiofs shares; volumes → raw block devices. The
        // mapping/validation is host-side; the attach FFI is gated and
        // compile-verified under --features krunvm.
        for (idx, m) in opts.mounts.iter().enumerate() {
            // A deterministic, collision-free tag per mount. The guest agent
            // mounts `tag` at `m.target`.
            let tag = format!("ward-mnt-{idx}");
            self.krun_add_mount(ctx_id, &tag, std::path::Path::new(&m.source), m.readonly)?;
        }
        // Volume attach (ext4 images as block devices via krun_add_disk) is
        // deferred: the pinned libkrun 1.18.0 bottle exports no krun_add_disk*
        // symbols (built without block-device support). volume_ids are still
        // validated upstream; wiring them needs a block-capable libkrun build.
        let _ = &opts.volume_ids;

        // Boot the guest agent as the entry process and expose its vsock
        // over a host Unix socket so exec() can reach it. Compile-verified
        // under --features krunvm; runtime requires a booting microVM.
        self.krun_set_agent_entry(ctx_id)?;
        self.krun_add_agent_vsock(ctx_id, &self.agent_socket_path(&id))?;

        // Spawn the VM thread *after* all configuration calls. libkrun
        // requires krun_get_shutdown_eventfd + krun_start_enter to come
        // last. In stub builds this is a no-op; the state's `vm` field
        // doesn't exist.
        #[cfg(feature = "krunvm")]
        let vm = Some(self.krun_spawn_vm(&id, ctx_id)?);

        let now = std::time::SystemTime::now();
        let info = SandboxInfo {
            id: id.clone(),
            status: SandboxStatus::Creating,
            image: opts.image.clone(),
            created_at: now,
            ip_address: None,
            resources: opts.resources.clone(),
            expires_at: if opts.resources.timeout_seconds > 0 {
                Some(now + std::time::Duration::from_secs(opts.resources.timeout_seconds))
            } else {
                None
            },
        };

        let state = SandboxState {
            info: info.clone(),
            ctx_id,
            #[cfg(feature = "krunvm")]
            vm,
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
    /// Under `--features krunvm` this signals the VM thread to shut
    /// down via its eventfd, waits for the thread to exit (bounded),
    /// then frees the libkrun context. The order matters: freeing the
    /// context before the thread exits would race with the still-live
    /// `krun_start_enter` call.
    async fn remove_sandbox(&self, id: &str) -> Result<()> {
        #[allow(unused_mut)]
        let mut state = self
            .sandboxes
            .write()
            .await
            .remove(id)
            .ok_or_else(|| BackendError::NotFound(id.to_string()))?;

        // Signal + join the VM thread before freeing the context.
        #[cfg(feature = "krunvm")]
        if let Some(vm) = state.vm.take() {
            // Errors here are already logged inside the helper; we don't
            // surface them because the sandbox is being torn down and
            // any further failure on krun_free_ctx is the actionable one.
            let _ = self.krun_signal_and_join(id, vm).await;
        }

        if state.ctx_id != 0 {
            self.krun_free_ctx(state.ctx_id)?;
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
    /// pid per ADR-004) and logs a warning so misuse is visible.
    async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()> {
        let mut processes = self.processes.write().await;
        let Some(record) = processes.get(pid) else {
            return Ok(());
        };
        if record.sandbox_id != sandbox_id {
            tracing::warn!(
                caller_sandbox = %sandbox_id,
                pid = %pid,
                "kill_process: caller sandbox does not own pid; refusing without signalling"
            );
            return Ok(());
        }
        // Owner matches: remove + signal. Best-effort send — a closed
        // receiver just means the process already exited and its bridge
        // task is gone.
        if let Some(record) = processes.remove(pid) {
            let _ = record.kill_tx.send(()).await;
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
            let stream = tokio::net::UnixStream::connect(&sock).await.map_err(|e| {
                BackendError::Exec(format!("connect to guest agent at {}: {e}", sock.display()))
            })?;
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

impl KrunvmBackend {
    // -----------------------------------------------------------------------
    // Private krun FFI wrappers – all unsafe confined here
    // -----------------------------------------------------------------------

    fn krun_create_ctx(&self) -> Result<u32> {
        // SAFETY: krun_create_ctx() is always safe to call and returns a
        // non-negative context ID on success, or a negative errno on failure.
        #[cfg(feature = "krunvm")]
        {
            let ret = unsafe { super::krun_ffi::krun_create_ctx() };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_create_ctx failed: errno {}",
                    -ret
                )));
            }
            Ok(ret as u32)
        }
        #[cfg(not(feature = "krunvm"))]
        {
            // Stub: return a synthetic context ID for builds without krunvm.
            Ok(1)
        }
    }

    fn krun_free_ctx(&self, ctx_id: u32) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            // SAFETY: ctx_id came from krun_create_ctx and has not been freed.
            let ret = unsafe { super::krun_ffi::krun_free_ctx(ctx_id) };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_free_ctx failed: errno {}",
                    -ret
                )));
            }
        }
        let _ = ctx_id;
        Ok(())
    }

    fn krun_apply_resources(&self, ctx_id: u32, limits: &ResourceLimits) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            if limits.cpus > 0 {
                // libkrun's num_vcpus is uint8_t; values >255 cannot
                // round-trip and silent truncation would produce a
                // microVM with the wrong CPU count (or zero). Reject
                // explicitly.
                let num_vcpus: u8 = u8::try_from(limits.cpus).map_err(|_| {
                    BackendError::Internal(format!(
                        "cpus={} exceeds libkrun's uint8_t limit (255)",
                        limits.cpus
                    ))
                })?;
                // SAFETY: ctx_id came from krun_create_ctx and is live.
                let ret = unsafe {
                    super::krun_ffi::krun_set_vm_config(ctx_id, num_vcpus, limits.memory_mb)
                };
                if ret < 0 {
                    return Err(BackendError::Internal(format!(
                        "krun_set_vm_config failed: errno {}",
                        -ret
                    )));
                }
            }
        }
        let _ = (ctx_id, limits);
        Ok(())
    }

    fn krun_set_root(&self, ctx_id: u32, rootfs: &std::path::Path) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            use std::ffi::CString;
            let path = CString::new(rootfs.to_string_lossy().as_ref())
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            // SAFETY: path is a valid NUL-terminated C string; ctx_id is live.
            let ret = unsafe { super::krun_ffi::krun_set_root(ctx_id, path.as_ptr()) };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_set_root failed: errno {}",
                    -ret
                )));
            }
        }
        let _ = (ctx_id, rootfs);
        Ok(())
    }

    /// Set the guest agent as the microVM's entry process. libkrun runs one
    /// process per boot; ward boots the agent, which then serves exec
    /// requests over vsock for the sandbox's lifetime.
    ///
    /// The agent binary is expected at `/ward-agent` inside the rootfs;
    /// baking it there is part of the image/packaging pipeline.
    fn krun_set_agent_entry(&self, ctx_id: u32) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            use std::ffi::CString;
            let exec_path = CString::new(AGENT_GUEST_PATH)
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            // Empty argv/envp: a single null-pointer sentinel each.
            let argv: [*const std::ffi::c_char; 1] = [std::ptr::null()];
            let envp: [*const std::ffi::c_char; 1] = [std::ptr::null()];
            // SAFETY: ctx_id is live; exec_path is a valid C string; argv and
            // envp are null-terminated arrays as libkrun requires.
            let ret = unsafe {
                super::krun_ffi::krun_set_exec(
                    ctx_id,
                    exec_path.as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                )
            };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_set_exec failed: errno {}",
                    -ret
                )));
            }
        }
        let _ = ctx_id;
        Ok(())
    }

    /// Bridge the agent's guest vsock port to a host Unix socket. The daemon
    /// connects to `sock_path` to reach the agent listening on
    /// [`AGENT_VSOCK_PORT`] inside the guest.
    fn krun_add_agent_vsock(&self, ctx_id: u32, sock_path: &std::path::Path) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            use std::ffi::CString;
            let path = CString::new(sock_path.to_string_lossy().as_ref())
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            // `listen = false`: libkrun owns the host-side Unix socket and the
            // daemon connects to it; traffic is forwarded to the guest's
            // listening vsock port. (Boot-path semantics are verified once a
            // KVM runner exists; see the spec's verification ceiling.)
            // SAFETY: ctx_id is live; path is a valid C string.
            let ret = unsafe {
                super::krun_ffi::krun_add_vsock_port2(
                    ctx_id,
                    AGENT_VSOCK_PORT,
                    path.as_ptr(),
                    false,
                )
            };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_add_vsock_port2 failed: errno {}",
                    -ret
                )));
            }
        }
        let _ = (ctx_id, sock_path);
        Ok(())
    }

    /// Share a host directory into the guest via virtiofs. `tag` is the
    /// mount tag the guest uses to mount the share at the desired target.
    fn krun_add_mount(
        &self,
        ctx_id: u32,
        tag: &str,
        source: &std::path::Path,
        read_only: bool,
    ) -> Result<()> {
        #[cfg(feature = "krunvm")]
        {
            use std::ffi::CString;
            let c_tag = CString::new(tag).map_err(|e| BackendError::Internal(e.to_string()))?;
            let c_path = CString::new(source.to_string_lossy().as_ref())
                .map_err(|e| BackendError::Internal(e.to_string()))?;
            // shm_size 0 → libkrun's default DAX window.
            // SAFETY: ctx_id is live; both C strings are valid and NUL-terminated.
            let ret = unsafe {
                super::krun_ffi::krun_add_virtiofs3(
                    ctx_id,
                    c_tag.as_ptr(),
                    c_path.as_ptr(),
                    0,
                    read_only,
                )
            };
            if ret < 0 {
                return Err(BackendError::Internal(format!(
                    "krun_add_virtiofs3 failed for {tag}: errno {}",
                    -ret
                )));
            }
        }
        let _ = (ctx_id, tag, source, read_only);
        Ok(())
    }

    /// Host-side Unix socket path bridging to the sandbox's agent vsock.
    fn agent_socket_path(&self, sandbox_id: &str) -> std::path::PathBuf {
        self.data_dir
            .join("sandboxes")
            .join(sandbox_id)
            .join("agent.sock")
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

    /// Capture the shutdown eventfd and spawn the dedicated OS thread
    /// that runs `krun_start_enter`. Must be called only after every
    /// `krun_set_*` configuration call for the context.
    ///
    /// `krun_start_enter` blocks for the lifetime of the microVM, so we
    /// do **not** put it on a tokio task or `spawn_blocking` pool slot.
    /// A bare `std::thread::spawn` is the documented contract.
    ///
    /// Returns the runtime metadata to be stored in `SandboxState`. The
    /// thread logs its own exit via `tracing` so failures surface in
    /// the daemon log without the caller needing to await the handle.
    #[cfg(feature = "krunvm")]
    fn krun_spawn_vm(&self, sandbox_id: &str, ctx_id: u32) -> Result<VmRuntime> {
        // SAFETY: ctx_id came from krun_create_ctx and is configured but
        // not yet entered. Per libkrun's contract, this must be called
        // before krun_start_enter; calling it after is a use-after-enter
        // bug. We enforce ordering by sequencing here in create_sandbox.
        let shutdown_fd = unsafe { super::krun_ffi::krun_get_shutdown_eventfd(ctx_id) };
        if shutdown_fd < 0 {
            // Negative is documented for some platforms (e.g. macOS may
            // not have a usable eventfd). Don't fail the boot. Just
            // accept that remove_sandbox won't be able to signal shutdown
            // cleanly. Log so the operator knows.
            tracing::warn!(
                sandbox = %sandbox_id,
                ctx_id,
                ret = shutdown_fd,
                "krun_get_shutdown_eventfd returned negative; shutdown signalling disabled"
            );
        }

        let sandbox_id_for_thread = sandbox_id.to_string();
        let thread = std::thread::Builder::new()
            .name(format!("krun-vm-{sandbox_id}"))
            .spawn(move || {
                tracing::info!(
                    sandbox = %sandbox_id_for_thread,
                    ctx_id,
                    "krun_start_enter starting"
                );
                // SAFETY: ctx_id is live and fully configured. This call
                // blocks until the VM exits, either because guest init
                // ran to completion, or because the shutdown eventfd was
                // poked, or because libkrun failed early (missing init,
                // bad rootfs, KVM unavailable, ...).
                let ret = unsafe { super::krun_ffi::krun_start_enter(ctx_id) };
                if ret < 0 {
                    tracing::error!(
                        sandbox = %sandbox_id_for_thread,
                        ctx_id,
                        errno = -ret,
                        "krun_start_enter failed"
                    );
                } else {
                    tracing::info!(
                        sandbox = %sandbox_id_for_thread,
                        ctx_id,
                        code = ret,
                        "krun_start_enter returned"
                    );
                }
                ret
            })
            .map_err(|e| BackendError::Internal(format!("spawn VM thread failed: {e}")))?;

        Ok(VmRuntime {
            thread,
            shutdown_fd,
        })
    }

    /// Poke the shutdown eventfd, wait for the VM thread to exit, and
    /// reap the thread. Best-effort: a stuck VM is detached after the
    /// timeout rather than blocking `remove_sandbox` indefinitely.
    ///
    /// Returns the thread's exit code on a clean reap; `None` if we
    /// timed out and detached the thread. Errors only on truly
    /// catastrophic failures (none currently).
    #[cfg(feature = "krunvm")]
    async fn krun_signal_and_join(&self, sandbox_id: &str, vm: VmRuntime) -> Result<Option<i32>> {
        // 1. Signal: write u64(1) to the eventfd. Best-effort. If the
        //    fd is bogus or write fails, we still try to join in case
        //    the VM is already on its way out for other reasons (guest
        //    poweroff, OOM, etc).
        if vm.shutdown_fd >= 0 {
            let val: u64 = 1;
            // SAFETY: shutdown_fd was obtained from krun_get_shutdown_eventfd
            // and is valid until krun_free_ctx (which we call after this).
            // We're writing exactly 8 bytes from a stack u64.
            let n = unsafe {
                write(
                    vm.shutdown_fd,
                    &val as *const u64 as *const std::ffi::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
            if n < 0 {
                tracing::warn!(
                    sandbox = %sandbox_id,
                    fd = vm.shutdown_fd,
                    "write(shutdown_eventfd) failed; will still attempt to join"
                );
            }
        }

        // 2. Bounded join. `JoinHandle::join` is unbounded. If libkrun
        //    or the host wedges, we'd block forever. Poll `is_finished`
        //    instead, capped at JOIN_TIMEOUT.
        const JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
        let start = std::time::Instant::now();
        while start.elapsed() < JOIN_TIMEOUT {
            if vm.thread.is_finished() {
                let ret = vm
                    .thread
                    .join()
                    .map_err(|_| BackendError::Internal("VM thread panicked".into()))?;
                return Ok(Some(ret));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }

        // Timeout: detach. The thread keeps running but the daemon
        // continues; libkrun resource leak is acknowledged but the
        // alternative (block remove_sandbox forever) is worse. Operator
        // visibility via log.
        tracing::warn!(
            sandbox = %sandbox_id,
            timeout_s = JOIN_TIMEOUT.as_secs(),
            "VM thread did not exit within timeout; detaching (libkrun context will leak)"
        );
        drop(vm.thread);
        Ok(None)
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

    // SEC-014: atomic swap via renameat2(RENAME_EXCHANGE) on Linux —
    // dest gets new contents, staging gets old contents (which we then
    // drop). EXCHANGE requires both paths to exist; first-time restore
    // (no dest yet) falls back to a plain rename. macOS lacks an
    // equivalent atomic-exchange syscall; the unlink+rename two-step
    // residual remains on Darwin (documented at the call site).
    #[cfg(target_os = "linux")]
    {
        if dest.exists() {
            use rustix::fs::{CWD, RenameFlags, renameat_with};
            renameat_with(CWD, &staging, CWD, dest, RenameFlags::EXCHANGE).map_err(
                |e: rustix::io::Errno| BackendError::Internal(format!("renameat2 EXCHANGE: {e}")),
            )?;
            // Drop the old contents that landed in staging post-swap.
            let _ = std::fs::remove_dir_all(&staging);
        } else {
            std::fs::rename(&staging, dest).map_err(BackendError::Io)?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Darwin residual: a crash between unlink and rename leaves
        // `dest` absent (sandbox unbootable on next boot). renamex_np
        // with RENAME_SWAP exists on macOS 10.12+ but rustix doesn't
        // expose it yet; tracked as a follow-up.
        if dest.exists() {
            std::fs::remove_dir_all(dest).map_err(BackendError::Io)?;
        }
        std::fs::rename(&staging, dest).map_err(BackendError::Io)?;
    }
    Ok(())
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
    use crate::protocol::{CommunicationPolicy, CreateOpts, EgressPolicy};
    use pretty_assertions::assert_eq;

    /// Build a fresh backend rooted in a tempdir. Leaks the TempDir
    /// intentionally so the data_dir survives the lifetime of the
    /// returned backend across async boundaries.
    fn backend_in_tempdir() -> KrunvmBackend {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        KrunvmBackend::new(path)
    }

    fn create_opts() -> CreateOpts {
        CreateOpts {
            image: "alpine:latest".into(),
            mounts: vec![],
            volume_ids: vec![],
            egress: EgressPolicy::default(),
            resources: ResourceLimits::default(),
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
}
