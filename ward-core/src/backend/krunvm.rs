// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! MicroVM backend using libkrun via krun-sys.
//!
//! All `unsafe` calls to the krun-sys C bindings are confined to this module.
//! The public API is fully safe Rust.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use super::{BackendError, ProcessHandle, Result};
use crate::protocol::{CreateOpts, EgressMode, ResourceLimits, SandboxInfo, SandboxStatus};

// ---------------------------------------------------------------------------
// Per-sandbox state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SandboxState {
    info: SandboxInfo,
    /// krun context ID returned by krun_create_ctx().
    /// 0 means not yet started.
    ctx_id: u32,
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
    data_dir: std::path::PathBuf,
}

impl KrunvmBackend {
    pub fn new(data_dir: std::path::PathBuf) -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            data_dir,
        }
    }

    /// Create a new sandbox and start the microVM.
    pub async fn create_sandbox(&self, id: String, opts: &CreateOpts) -> Result<SandboxInfo> {
        let ctx_id = self.krun_create_ctx()?;
        self.krun_apply_resources(ctx_id, &opts.resources)?;

        let rootfs = self
            .data_dir
            .join("sandboxes")
            .join(&id)
            .join("rootfs");

        self.krun_set_root(ctx_id, &rootfs)?;

        if opts.egress.mode != EgressMode::Deny {
            // TODO: configure virtio-net and attach egress proxy TAP.
        }

        // TODO: apply mount points.
        // TODO: call krun_start_enter in a dedicated thread.

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
        };

        self.sandboxes.write().await.insert(id, state);
        Ok(info)
    }

    /// Retrieve sandbox info by ID.
    pub async fn get_sandbox(&self, id: &str) -> Result<SandboxInfo> {
        self.sandboxes
            .read()
            .await
            .get(id)
            .map(|s| s.info.clone())
            .ok_or_else(|| BackendError::NotFound(id.to_string()))
    }

    /// List all sandboxes.
    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>> {
        Ok(self
            .sandboxes
            .read()
            .await
            .values()
            .map(|s| s.info.clone())
            .collect())
    }

    /// Stop and remove a sandbox.
    pub async fn remove_sandbox(&self, id: &str) -> Result<()> {
        let state = self
            .sandboxes
            .write()
            .await
            .remove(id)
            .ok_or_else(|| BackendError::NotFound(id.to_string()))?;

        if state.ctx_id != 0 {
            self.krun_free_ctx(state.ctx_id)?;
        }
        Ok(())
    }

    /// Count of active sandboxes.
    pub async fn count(&self) -> Result<usize> {
        Ok(self.sandboxes.read().await.len())
    }

    /// Exec a command inside a running sandbox.
    pub async fn exec(
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
        let _ = command;

        Ok(ProcessHandle {
            pid,
            sandbox_id: sandbox_id.to_string(),
            stdin_tx: None,
            output_rx: None,
        })
    }

    // -----------------------------------------------------------------------
    // Private krun FFI wrappers – all unsafe confined here
    // -----------------------------------------------------------------------

    fn krun_create_ctx(&self) -> Result<u32> {
        // SAFETY: krun_create_ctx() is always safe to call and returns a
        // non-negative context ID on success, or a negative errno on failure.
        #[cfg(feature = "krunvm")]
        {
            let ret = unsafe { krun_sys::krun_create_ctx() };
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
            let ret = unsafe { krun_sys::krun_free_ctx(ctx_id) };
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
                // SAFETY: valid ctx_id, cpus is a plain u32.
                let ret = unsafe { krun_sys::krun_set_vm_config(ctx_id, limits.cpus, limits.memory_mb) };
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
            let ret = unsafe { krun_sys::krun_set_root(ctx_id, path.as_ptr()) };
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
}
