// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Shared setup for the benches: an offline stub backend wired into a
//! `SandboxManager` and a `VolumeManager`, with no network or disk images.

use std::path::Path;
use std::sync::Arc;

use ward_core::backend::Backend;
use ward_core::backend::BackendError;
use ward_core::backend::image::{ImagePuller, ImageStore, PulledImage};
use ward_core::backend::krunvm::KrunvmBackend;
use ward_core::comms::Broker;
use ward_core::config::NetworkBackendChoice;
use ward_core::sandbox::SandboxManager;
use ward_core::volume::VolumeManager;

const IMAGE_CACHE_CAPACITY: usize = 64;
const MAX_SANDBOXES: usize = 8;
const MAX_VOLUMES: usize = 4;

/// Creates a minimal rootfs without touching the network.
#[derive(Debug)]
struct FakePuller;

#[async_trait::async_trait]
impl ImagePuller for FakePuller {
    async fn pull(&self, _reference: &str, dest: &Path) -> Result<PulledImage, BackendError> {
        std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
        Ok(PulledImage::without_config("sha256:bench".to_string()))
    }
}

/// Build the managers on the stub backend under `data_dir`.
pub fn managers(data_dir: &Path) -> (Arc<SandboxManager>, Arc<VolumeManager>) {
    let cache_dir = data_dir.join("cache").join("images");
    let store = Arc::new(ImageStore::with_puller(
        cache_dir,
        IMAGE_CACHE_CAPACITY,
        Arc::new(FakePuller),
    ));
    let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::with_image_store_for_test(
        data_dir.to_path_buf(),
        store,
    ));
    let sandboxes = Arc::new(SandboxManager::new(
        backend,
        Arc::new(Broker::new()),
        MAX_SANDBOXES,
        /* allow_host_mounts = */ false,
        NetworkBackendChoice::Smoltcp,
    ));
    let volumes = Arc::new(VolumeManager::new(data_dir.to_path_buf(), MAX_VOLUMES));
    (sandboxes, volumes)
}
