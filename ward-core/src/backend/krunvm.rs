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
    CreateOpts, EgressMode, ResourceLimits, SandboxInfo, SandboxStatus, SnapshotInfo, StreamEvent,
    StreamEventKind,
};

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
#[derive(Debug)]
pub struct KrunvmBackend {
    sandboxes: Arc<RwLock<HashMap<String, SandboxState>>>,
    /// Snapshot metadata keyed by snapshot_id. In stub mode this is a
    /// plain in-memory map; the real backend will pair this with on-disk
    /// libkrun checkpoint state under data_dir/snapshots/.
    snapshots: Arc<RwLock<HashMap<String, SnapshotInfo>>>,
    data_dir: std::path::PathBuf,
}

impl KrunvmBackend {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
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

        let rootfs = self.data_dir.join("sandboxes").join(&id).join("rootfs");

        self.krun_set_root(ctx_id, &rootfs)?;

        if opts.egress.mode != EgressMode::Deny {
            // TODO: configure virtio-net and attach egress proxy TAP.
        }

        // TODO: apply mount points.

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
        // dangling reference no caller could ever reach.
        self.snapshots
            .write()
            .await
            .retain(|_, snap| snap.sandbox_id != id);

        Ok(())
    }

    /// Count of active sandboxes.
    async fn count(&self) -> Result<usize> {
        Ok(self.sandboxes.read().await.len())
    }

    /// Signal a process to terminate. The stub does no real work because
    /// the stub's "process" is just a pair of mpsc channels — the manager
    /// closes them by dropping the ProcessRecord. The real backend will
    /// send SIGTERM/SIGKILL over vsock here; the public signature stays
    /// the same so the manager and gRPC layer never need to change.
    async fn kill_process(&self, _sandbox_id: &str, _pid: &str) -> Result<()> {
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

    /// Take a snapshot of a sandbox's current state.
    /// Returns the SnapshotInfo with a freshly-minted snapshot_id.
    async fn create_snapshot(&self, sandbox_id: &str, label: &str) -> Result<SnapshotInfo> {
        // Sandbox must exist — snapshotting a non-existent sandbox is a
        // user error, not an internal failure.
        if !self.sandboxes.read().await.contains_key(sandbox_id) {
            return Err(BackendError::NotFound(sandbox_id.to_string()));
        }

        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let info = SnapshotInfo {
            snapshot_id: snapshot_id.clone(),
            sandbox_id: sandbox_id.to_string(),
            label: label.to_string(),
            created_at: std::time::SystemTime::now(),
            // Real backend will report the checkpoint blob size. Zero
            // is the truthful answer for a stub that materialises nothing.
            size_bytes: 0,
        };
        self.snapshots
            .write()
            .await
            .insert(snapshot_id, info.clone());
        Ok(info)
    }

    /// Restore a sandbox from a previously-taken snapshot. The stub
    /// verifies the snapshot exists AND belongs to the named sandbox;
    /// the real backend will additionally swap the VM's rootfs and
    /// resume execution from the checkpoint.
    async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()> {
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

        // TODO: use krun_exec / vsock channel to run the command inside the VM.
        // For now the stub produces a tiny scripted output stream so that
        // StreamOutput has something to deliver end-to-end. Tests assert on
        // this scripted shape; when the real krun exec lands the producer
        // task is replaced but the channel contract is unchanged.
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

        // Stdin half: caller writes via stdin_tx; the drain task here keeps
        // stdin_rx alive so writes don't immediately fail with "channel
        // closed". Bytes are discarded — the real backend will pipe them
        // into the VM over vsock. The drain task exits when ProcessRecord
        // is dropped and stdin_tx with it.
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(16);
        tokio::spawn(async move {
            while let Some(_chunk) = stdin_rx.recv().await {
                // discard: real backend would forward to the VM's stdin
            }
        });

        Ok(ProcessHandle {
            pid,
            sandbox_id: sandbox_id.to_string(),
            stdin_tx: Some(stdin_tx),
            output_rx: Some(output_rx),
        })
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
// Tests: snapshot stub behaviour
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
        assert_eq!(snap.size_bytes, 0); // stub doesn't materialise blobs
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
}
