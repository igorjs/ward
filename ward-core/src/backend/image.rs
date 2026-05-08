// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! OCI image pull, unpack, and local cache management.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;

use super::{BackendError, Result};

// ---------------------------------------------------------------------------
// Cached image entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CachedImage {
    /// OCI image reference (e.g. "python:3.12-slim").
    pub reference: String,
    /// Path to the unpacked rootfs on disk.
    pub rootfs_path: PathBuf,
    /// Digest of the pulled manifest.
    pub digest: String,
    pub pulled_at: std::time::SystemTime,
}

// ---------------------------------------------------------------------------
// ImageStore
// ---------------------------------------------------------------------------

/// Manages the local OCI image cache: pull, unpack, and serve rootfs paths.
#[derive(Debug)]
pub struct ImageStore {
    cache_dir: PathBuf,
    images: Arc<RwLock<HashMap<String, CachedImage>>>,
}

impl ImageStore {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            images: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Return the rootfs path for an image, pulling and unpacking if needed.
    pub async fn ensure(&self, reference: &str) -> Result<PathBuf> {
        // Fast path: already cached.
        if let Some(entry) = self.images.read().await.get(reference) {
            return Ok(entry.rootfs_path.clone());
        }

        // Slow path: pull, unpack, cache.
        let rootfs = self.pull_and_unpack(reference).await?;

        let entry = CachedImage {
            reference: reference.to_string(),
            rootfs_path: rootfs.clone(),
            digest: "sha256:TODO".to_string(),
            pulled_at: std::time::SystemTime::now(),
        };
        self.images
            .write()
            .await
            .insert(reference.to_string(), entry);

        Ok(rootfs)
    }

    /// Check whether an image is present in the local cache.
    pub async fn is_cached(&self, reference: &str) -> bool {
        self.images.read().await.contains_key(reference)
    }

    /// Remove an image from the cache, deleting its rootfs directory.
    pub async fn remove(&self, reference: &str) -> Result<()> {
        let entry = self
            .images
            .write()
            .await
            .remove(reference)
            .ok_or_else(|| BackendError::Image(format!("not cached: {}", reference)))?;

        if entry.rootfs_path.exists() {
            std::fs::remove_dir_all(&entry.rootfs_path).map_err(BackendError::Io)?;
        }
        Ok(())
    }

    /// List all cached images.
    pub async fn list(&self) -> Vec<CachedImage> {
        self.images.read().await.values().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn pull_and_unpack(&self, reference: &str) -> Result<PathBuf> {
        // TODO: implement actual OCI pull via oci-distribution or skopeo.
        // For now this is a stub that creates an empty rootfs directory so
        // the rest of the system can proceed in development/testing.

        let slug = reference.replace([':', '/'], "_");
        let rootfs = self.cache_dir.join(&slug).join("rootfs");
        tokio::fs::create_dir_all(&rootfs)
            .await
            .map_err(BackendError::Io)?;

        tracing::warn!(
            reference,
            ?rootfs,
            "image pull not yet implemented – using empty rootfs stub"
        );

        Ok(rootfs)
    }

    /// Validate that a path looks like a usable rootfs.
    fn _validate_rootfs(path: &Path) -> Result<()> {
        if !path.join("bin").exists() && !path.join("usr").exists() {
            return Err(BackendError::Image(format!(
                "rootfs at {} appears incomplete (no /bin or /usr)",
                path.display()
            )));
        }
        Ok(())
    }
}
