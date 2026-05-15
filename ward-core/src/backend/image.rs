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

        // Use a UUID-based directory name instead of deriving from the image
        // reference. Deriving from user input (e.g. reference.replace('/', "_"))
        // is vulnerable to path traversal if the reference contains ".." or
        // other control sequences that survive the replacement.
        let dir_name = uuid::Uuid::new_v4().to_string();
        let rootfs = self.cache_dir.join(&dir_name).join("rootfs");
        tokio::fs::create_dir_all(&rootfs)
            .await
            .map_err(BackendError::Io)?;

        tracing::warn!(
            reference,
            dir = %dir_name,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    /// Build a fresh ImageStore rooted in a tempdir. The tempdir's `Drop`
    /// cleans up at the end of the test, so each test stays hermetic and
    /// never leaks state across runs.
    fn store_in_tempdir() -> (ImageStore, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::new(tmp.path().to_path_buf());
        (store, tmp)
    }

    // ----- is_cached: initial state --------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_is_cached_then_returns_false() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let cached = store.is_cached("alpine:latest").await;

        // Assert
        assert!(!cached);
    }

    // ----- ensure: happy path --------------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_ensure_then_returns_existing_rootfs_path() {
        // Arrange
        let (store, tmp) = store_in_tempdir();

        // Act
        let rootfs = store.ensure("alpine:latest").await.expect("ensure");

        // Assert: the returned path lives under the cache dir AND was
        // materialised on disk by `pull_and_unpack`. Both matter — callers
        // pass this path to libkrun which will fail on missing dirs.
        assert!(rootfs.starts_with(tmp.path()));
        assert!(rootfs.exists());
        assert!(rootfs.ends_with("rootfs"));
    }

    #[tokio::test]
    async fn given_fresh_store_when_ensure_then_marks_image_cached() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        store.ensure("alpine:latest").await.expect("ensure");

        // Assert: the read-through of `is_cached` reflects the write.
        assert!(store.is_cached("alpine:latest").await);
    }

    #[tokio::test]
    async fn given_cached_image_when_ensure_again_then_returns_same_path() {
        // Arrange: idempotency is critical because the slow path generates
        // a fresh UUID directory each call. If the fast path ever broke,
        // we'd silently leak rootfs dirs on every restart.
        let (store, _tmp) = store_in_tempdir();
        let first = store.ensure("alpine:latest").await.expect("first ensure");

        // Act
        let second = store.ensure("alpine:latest").await.expect("second ensure");

        // Assert
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn given_two_distinct_references_when_ensure_then_each_gets_unique_path() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let alpine = store.ensure("alpine:latest").await.expect("alpine");
        let python = store.ensure("python:3.12-slim").await.expect("python");

        // Assert: separate cache entries → separate rootfs dirs. Sharing
        // would corrupt one sandbox when another image is removed.
        assert_ne!(alpine, python);
        assert!(store.is_cached("alpine:latest").await);
        assert!(store.is_cached("python:3.12-slim").await);
    }

    // ----- remove --------------------------------------------------------

    #[tokio::test]
    async fn given_cached_image_when_remove_then_directory_deleted_and_not_cached() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();
        let rootfs = store.ensure("alpine:latest").await.expect("ensure");
        assert!(rootfs.exists());

        // Act
        store.remove("alpine:latest").await.expect("remove");

        // Assert: cache forgot it AND the on-disk dir is gone. Both halves
        // matter — leaking either causes disk creep over time.
        assert!(!store.is_cached("alpine:latest").await);
        assert!(!rootfs.exists());
    }

    #[tokio::test]
    async fn given_uncached_reference_when_remove_then_returns_image_error() {
        // Arrange: removing something that was never cached.
        let (store, _tmp) = store_in_tempdir();

        // Act
        let err = store.remove("ghost:latest").await.expect_err("remove");

        // Assert: must be Image, NOT NotFound — the SandboxManager reserves
        // NotFound for sandbox identity and translates it to gRPC
        // NotFound. Image errors map to Internal, which is the right
        // signal for "your cache is in a weird state".
        match err {
            BackendError::Image(msg) => assert!(msg.contains("ghost:latest"), "got: {msg}"),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    // ----- list ----------------------------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_list_then_returns_empty() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let entries = store.list().await;

        // Assert
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn given_multiple_ensures_when_list_then_returns_all_entries() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("alpine");
        store.ensure("python:3.12-slim").await.expect("python");

        // Act
        let mut refs: Vec<String> = store.list().await.into_iter().map(|c| c.reference).collect();
        refs.sort();

        // Assert: order isn't guaranteed by HashMap iteration, so we sort
        // before comparing. This is the contract callers should rely on too.
        assert_eq!(refs, vec!["alpine:latest", "python:3.12-slim"]);
    }

    // ----- security: path traversal regression guard ---------------------

    #[tokio::test]
    async fn given_reference_with_traversal_when_ensure_then_path_stays_under_cache_dir() {
        // Arrange: a malicious reference that would escape the cache dir
        // if `pull_and_unpack` derived its directory name from the
        // reference (e.g. via `reference.replace('/', "_")`). The fix in
        // pull_and_unpack uses a UUID, so this stays under the cache.
        let (store, tmp) = store_in_tempdir();

        // Act
        let rootfs = store
            .ensure("../../../etc/passwd")
            .await
            .expect("ensure");

        // Assert: canonicalise both sides so symlinks (macOS /private/var)
        // don't trip the comparison.
        let canonical_root = rootfs.canonicalize().expect("canonical rootfs");
        let canonical_tmp = tmp.path().canonicalize().expect("canonical tmp");
        assert!(
            canonical_root.starts_with(&canonical_tmp),
            "rootfs {} escaped tempdir {}",
            canonical_root.display(),
            canonical_tmp.display(),
        );
    }

    // ----- CachedImage metadata is populated -----------------------------

    #[tokio::test]
    async fn given_ensure_when_list_then_entry_carries_reference_and_digest() {
        // Arrange: digest is a stub today but the field is part of the
        // contract; tests guard against accidental wipes.
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("ensure");

        // Act
        let entry = store
            .list()
            .await
            .into_iter()
            .find(|c| c.reference == "alpine:latest")
            .expect("entry present");

        // Assert
        assert_eq!(entry.reference, "alpine:latest");
        assert!(!entry.digest.is_empty());
    }
}
