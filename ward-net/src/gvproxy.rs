// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! gvproxy backend.
//!
//! Per ADR-018, gvproxy is the v0.2 alternative for hosts that already run
//! podman-machine (which ships gvproxy). Unlike passt (which uses a
//! socketpair FD), gvproxy listens on a Unix datagram socket path and
//! libkrun connects to it — so the wiring is path-based, not FD-based.
//!
//! This module owns three concerns:
//!
//! 1. **Probe** — confirm `gvproxy` is on `$PATH`. Falls back with a
//!    clear `Error::DependencyMissing` hint pointing at `docs/rootless.md`.
//! 2. **Command-line construction** — translate the sandbox ID into the
//!    gvproxy flags the daemon needs. Pure function so it's unit-testable
//!    without spawning anything.
//! 3. **Lifecycle** — spawn + supervise + reap the gvproxy subprocess.
//!    The path injection into libkrun lives in `ward-core` (it needs the
//!    krun_ctx_id), so this crate exposes a `spawn_for_sandbox` that
//!    returns the socket path; the caller injects it into libkrun.
//!
//! ## Socket scheme
//!
//! gvproxy uses `-listen-vfkit unixgram://<path>` for vfkit-compatible
//! applications. libkrun's `krun_set_gvproxy_path` connects to that same
//! path. The socket lives under `runtime_dir(data_dir)` and is named
//! `gvproxy-<sandbox_id>.sock`.

use std::path::PathBuf;
use std::sync::RwLock;

use crate::{AttachId, AttachOptions, Error, NetworkBackend, runtime_dir};

/// Name of the gvproxy binary we probe for.
const GVPROXY_BIN: &str = "gvproxy";

/// Live gvproxy subprocess for one sandbox.
///
/// Created by [`spawn_for_sandbox`]. The caller (ward-core) extracts
/// `socket_path` and hands it to `krun_set_gvproxy_path`; this struct retains
/// the child handle so gvproxy stays alive until the sandbox is torn down.
#[derive(Debug)]
pub struct GvproxyHandle {
    /// Opaque ID used by the [`NetworkBackend`] trait's attach/detach path.
    pub attach_id: String,
    /// The Unix datagram socket path gvproxy is listening on.
    /// Pass to `krun_set_gvproxy_path` so libkrun can connect.
    pub socket_path: PathBuf,
    /// The live gvproxy child process. Use [`GvproxyHandle::kill`] to SIGTERM
    /// and reap it during sandbox teardown.
    pub child: tokio::process::Child,
}

impl GvproxyHandle {
    /// SIGTERM the gvproxy child and await its exit. Idempotent: a
    /// process that has already exited will have `try_wait` return
    /// `Ok(Some(_))`, and we return `Ok(())` without sending SIGTERM.
    pub async fn kill(&mut self) -> Result<(), Error> {
        // If already exited, reap and return.
        match self.child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("gvproxy try_wait error: {e}");
            }
        }
        if let Err(e) = self.child.start_kill() {
            // ESRCH = already gone; treat as success.
            tracing::debug!("gvproxy start_kill: {e}");
        }
        let _ = self.child.wait().await;
        Ok(())
    }
}

/// Spawn `gvproxy` with `-listen-vfkit unixgram://<socket_path>` and return
/// a [`GvproxyHandle`] holding the socket path and the live child.
///
/// The socket lives under `runtime_dir(None)` and is named
/// `gvproxy-<sandbox_id>.sock`. The runtime directory is created if it
/// does not yet exist.
///
/// # Errors
///
/// Returns [`Error::DependencyMissing`] if `gvproxy` is not on `$PATH`.
/// Returns [`Error::Process`] if creating the runtime directory or spawning
/// the child fails.
pub async fn spawn_for_sandbox(
    sandbox_id: &str,
    _opts: &AttachOptions,
) -> Result<GvproxyHandle, Error> {
    // Probe first so error message is actionable.
    GvproxyBackend::default().probe().await?;

    // Determine the socket path under the runtime directory.
    let dir = runtime_dir(None);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        Error::Process(format!(
            "failed to create runtime dir {}: {e}",
            dir.display()
        ))
    })?;

    let socket_path = dir.join(format!("gvproxy-{sandbox_id}.sock"));

    // Remove a stale socket from a previous crashed run. gvproxy refuses to
    // start if the path already exists.
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    // The vfkit listen address must use the unixgram:// scheme.
    // gvproxy validates this and will exit if any other scheme is given.
    let listen_addr = format!("unixgram://{}", socket_path.display());

    let child = tokio::process::Command::new(GVPROXY_BIN)
        .arg("-listen-vfkit")
        .arg(&listen_addr)
        .spawn()
        .map_err(|e| Error::Process(format!("failed to spawn gvproxy: {e}")))?;

    let attach_id = format!("gvproxy:{sandbox_id}");

    Ok(GvproxyHandle {
        attach_id,
        socket_path,
        child,
    })
}

#[derive(Debug, Default)]
pub struct GvproxyBackend {
    /// Map of attach_id -> child pid. Lookup table so detach can SIGTERM
    /// the right gvproxy process. RwLock is fine — attach/detach are not
    /// hot-path operations.
    children: RwLock<std::collections::HashMap<AttachId, u32>>,
}

#[async_trait::async_trait]
impl NetworkBackend for GvproxyBackend {
    fn name(&self) -> &'static str {
        "gvproxy"
    }

    async fn probe(&self) -> Result<(), Error> {
        match which::which(GVPROXY_BIN) {
            Ok(p) => {
                tracing::debug!(path = %p.display(), "gvproxy binary found");
                Ok(())
            }
            Err(_) => Err(Error::DependencyMissing {
                what: format!(
                    "{GVPROXY_BIN}(1) — install via podman-machine or \
                     see docs/rootless.md for setup instructions"
                ),
            }),
        }
    }

    async fn attach(&self, sandbox_id: &str, opts: &AttachOptions) -> Result<AttachId, Error> {
        // The actual path-injection into libkrun lives in ward-core because
        // it needs the krun_ctx_id, which this crate does not see. Real spawn
        // is deferred to the integration layer; this method records the attach
        // so detach has something to find. See ADR-018 "Implementation".
        let attach_id = format!("gvproxy:{sandbox_id}");
        let _ = opts;
        // Record a placeholder pid (0) so the map shape is stable. The real
        // spawn integration will replace this with the actual child pid.
        self.children
            .write()
            .map_err(|e| Error::Process(format!("attach lock poisoned: {e}")))?
            .insert(attach_id.clone(), 0);
        Ok(attach_id)
    }

    async fn detach(&self, attach_id: &AttachId) -> Result<(), Error> {
        let pid = self
            .children
            .write()
            .map_err(|e| Error::Process(format!("detach lock poisoned: {e}")))?
            .remove(attach_id);
        // Idempotent: detaching an unknown id is fine.
        let _ = pid;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn given_attach_then_detach_when_detach_again_then_idempotent() {
        let b = GvproxyBackend::default();
        let id = b.attach("sb-g1", &AttachOptions::default()).await.unwrap();
        b.detach(&id).await.unwrap();
        // Detaching an unknown id is fine.
        b.detach(&id).await.unwrap();
    }
}
