// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Embedded ward runtime.
//!
//! `ward-runtime` lets a process boot libkrun-backed sandboxes in-process
//! without a separate `wardd` daemon. SDKs depend on this crate to provide
//! `Sandbox::builder(...).create()` style APIs that work with zero
//! infrastructure setup. The `wardd` binary uses the same `Runtime` and
//! layers a gRPC server on top so multi-process / fleet use cases keep
//! working.
//!
//! See `docs/adr/016-embedded-mode-microvms.md` for the design rationale.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> Result<(), ward_runtime::Error> {
//! use ward_runtime::Runtime;
//!
//! let runtime = Runtime::builder()
//!     .data_dir("/tmp/ward-embedded")
//!     .max_sandboxes(8)
//!     .build()
//!     .await?;
//!
//! // The sandbox manager is the same one wardd uses; call it directly.
//! let _mgr = runtime.sandbox_manager();
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ward_core::backend::Backend;
use ward_core::backend::krunvm::KrunvmBackend;
use ward_core::comms::Broker;
use ward_core::config::Config;
use ward_core::sandbox::SandboxManager;
use ward_core::volume::VolumeManager;

/// Errors that can occur while booting an embedded runtime.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to create the data directory tree.
    #[error("failed to prepare data directory: {0}")]
    DataDir(#[from] std::io::Error),
}

/// An in-process ward runtime.
///
/// Owns the backend, broker, sandbox manager, and volume manager. Drop
/// the `Runtime` to release in-flight sandboxes (best-effort: long-running
/// sandboxes may need explicit `remove` calls before drop for clean
/// teardown).
///
/// `Runtime` is cheap to share via `Clone` — internals are `Arc`s.
#[derive(Clone)]
pub struct Runtime {
    backend: Arc<dyn Backend>,
    broker: Arc<Broker>,
    sandbox_manager: Arc<SandboxManager>,
    volume_manager: Arc<VolumeManager>,
}

impl Runtime {
    /// Start building a runtime.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Construct a runtime from an already-resolved `Config`. Used by
    /// `wardd` to ensure the daemon and embedded paths build the same
    /// managers from the same sources.
    pub async fn from_config(cfg: &Config) -> Result<Self, Error> {
        cfg.ensure_dirs()?;
        Ok(Self::wire(
            cfg.data_dir.clone(),
            cfg.max_sandboxes,
            cfg.max_volumes,
            cfg.allow_host_mounts,
        ))
    }

    fn wire(
        data_dir: PathBuf,
        max_sandboxes: usize,
        max_volumes: usize,
        allow_host_mounts: bool,
    ) -> Self {
        let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::new(data_dir.clone()));
        let broker = Arc::new(Broker::new());
        let sandbox_manager = Arc::new(SandboxManager::new(
            Arc::clone(&backend),
            Arc::clone(&broker),
            max_sandboxes,
            allow_host_mounts,
        ));
        let volume_manager = Arc::new(VolumeManager::new(data_dir, max_volumes));
        Self {
            backend,
            broker,
            sandbox_manager,
            volume_manager,
        }
    }

    /// Shared reference to the sandbox manager — the primary entry point
    /// for create / exec / kill / snapshot operations.
    pub fn sandbox_manager(&self) -> Arc<SandboxManager> {
        Arc::clone(&self.sandbox_manager)
    }

    /// Shared reference to the volume manager.
    pub fn volume_manager(&self) -> Arc<VolumeManager> {
        Arc::clone(&self.volume_manager)
    }

    /// Shared reference to the cross-sandbox broker. Embedded users
    /// typically don't need this; it exists for the gRPC server in
    /// `wardd` which exposes publish / subscribe / log RPCs directly.
    pub fn broker(&self) -> Arc<Broker> {
        Arc::clone(&self.broker)
    }

    /// Shared reference to the underlying backend trait object. Held
    /// here so test harnesses can downcast or assert on backend state
    /// without going through the manager.
    pub fn backend(&self) -> Arc<dyn Backend> {
        Arc::clone(&self.backend)
    }
}

/// Builder for [`Runtime`]. Default values match the daemon's `Config`
/// defaults so embedded callers don't need to wire up env-var parsing.
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    data_dir: Option<PathBuf>,
    max_sandboxes: usize,
    max_volumes: usize,
    allow_host_mounts: bool,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            data_dir: None,
            max_sandboxes: 256,
            max_volumes: 256,
            allow_host_mounts: false,
        }
    }
}

impl RuntimeBuilder {
    /// Data directory for sandbox state, volumes, snapshots, and images.
    /// Required — there is no safe default that works across users and
    /// platforms for an embedded process.
    pub fn data_dir<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.data_dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Cap on concurrent sandboxes (default 256).
    pub fn max_sandboxes(mut self, n: usize) -> Self {
        self.max_sandboxes = n;
        self
    }

    /// Cap on persistent volumes (default 256).
    pub fn max_volumes(mut self, n: usize) -> Self {
        self.max_volumes = n;
        self
    }

    /// When true, bind-mount sources outside the ward-managed prefixes
    /// (`/home`, `/tmp`, `/var/lib/ward/`) are accepted. False by
    /// default — see SEC-020 / ADR-016. Only flip when the embedding
    /// application owns the entire host.
    pub fn allow_host_mounts(mut self, yes: bool) -> Self {
        self.allow_host_mounts = yes;
        self
    }

    /// Build the runtime. Ensures the data directory exists with
    /// owner-only (0700) perms before wiring the managers.
    pub async fn build(self) -> Result<Runtime, Error> {
        let data_dir = self.data_dir.ok_or_else(|| {
            Error::DataDir(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "data_dir is required; call RuntimeBuilder::data_dir(...)",
            ))
        })?;

        // SEC-002 / SEC-003 mirror: ensure the data tree before wiring
        // anything that writes into it. ward-core's Config::ensure_dirs
        // handles the O_NOFOLLOW + fchmod sequence; reuse it by
        // constructing a minimal Config for the directory contract.
        let cfg = Config {
            socket_path: data_dir.join("ward.sock"),
            data_dir: data_dir.clone(),
            log_level: String::new(),
            max_sandboxes: self.max_sandboxes,
            max_volumes: self.max_volumes,
            max_cached_images: 64,
            allow_host_mounts: self.allow_host_mounts,
            metrics_addr: None,
            network_backend: ward_core::config::NetworkBackendChoice::default(),
        };
        cfg.ensure_dirs()?;

        Ok(Runtime::wire(
            data_dir,
            self.max_sandboxes,
            self.max_volumes,
            self.allow_host_mounts,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn given_data_dir_when_build_then_runtime_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = Runtime::builder()
            .data_dir(tmp.path())
            .max_sandboxes(4)
            .build()
            .await
            .unwrap();
        // Smoke: managers are reachable.
        assert!(Arc::strong_count(&rt.sandbox_manager()) >= 2);
        assert!(Arc::strong_count(&rt.volume_manager()) >= 2);
    }

    #[tokio::test]
    async fn given_no_data_dir_when_build_then_invalid_input() {
        // Runtime intentionally doesn't derive Debug (Backend trait object
        // isn't Debug). Match the error variant directly instead of going
        // through unwrap_err.
        match Runtime::builder().build().await {
            Ok(_) => panic!("expected Error::DataDir, got Ok"),
            Err(Error::DataDir(io)) => {
                assert_eq!(io.kind(), std::io::ErrorKind::InvalidInput);
            }
        }
    }

    #[tokio::test]
    async fn given_config_when_from_config_then_managers_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            socket_path: tmp.path().join("ward.sock"),
            data_dir: tmp.path().to_path_buf(),
            log_level: "info".into(),
            max_sandboxes: 8,
            max_volumes: 8,
            max_cached_images: 8,
            allow_host_mounts: false,
            metrics_addr: None,
            network_backend: ward_core::config::NetworkBackendChoice::default(),
        };
        let rt = Runtime::from_config(&cfg).await.unwrap();
        assert!(Arc::strong_count(&rt.broker()) >= 2);
    }
}
