// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! VolumeManager: daemon-managed shared persistent volumes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::pb::{CreateVolumeRequest, VolumeInfo as PbVolumeInfo};
use crate::protocol::ApiError;

type Result<T> = std::result::Result<T, ApiError>;

/// Filename of the backing filesystem image inside a volume's directory.
const VOLUME_IMAGE: &str = "volume.img";

// ---------------------------------------------------------------------------
// Volume formatter
// ---------------------------------------------------------------------------

/// Allocates and formats a volume's backing filesystem image.
///
/// Split out as a trait so tests can inject a no-op (the real formatter
/// shells out to `mkfs.ext4`, which is Linux-only and unavailable on dev
/// macOS hosts and in cross-platform unit tests).
#[async_trait::async_trait]
pub trait VolumeFormatter: Send + Sync + std::fmt::Debug {
    /// Create a `size_mb`-megabyte filesystem image at `image_path`.
    async fn format(&self, image_path: &Path, size_mb: u32) -> Result<()>;
}

/// Production formatter: a sparse image sized with `truncate`-style
/// `set_len`, then formatted ext4 via `mkfs.ext4`.
#[derive(Debug, Default)]
pub struct Ext4Formatter;

#[async_trait::async_trait]
impl VolumeFormatter for Ext4Formatter {
    async fn format(&self, image_path: &Path, size_mb: u32) -> Result<()> {
        allocate_sparse_image(image_path, size_mb).await?;
        run_mkfs_ext4(image_path).await
    }
}

/// Create a sparse file of `size_mb` MiB at `path`. Sparse means the bytes
/// are not written up front: the file reports the full length but only
/// occupies disk as the guest writes into it.
async fn allocate_sparse_image(path: &Path, size_mb: u32) -> Result<()> {
    let len = u64::from(size_mb) * 1024 * 1024;
    // SEC-004: force 0600 on the backing image. Default umask yields 0644
    // which exposes the volume's filesystem blocks (and anything the guest
    // writes into them — secrets, keys, OCI image content) to other local
    // users on multi-user hosts.
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let file = opts
        .open(path)
        .await
        .map_err(|e| ApiError::Internal(format!("create volume image {}: {e}", path.display())))?;
    file.set_len(len)
        .await
        .map_err(|e| ApiError::Internal(format!("size volume image to {len} bytes: {e}")))?;
    Ok(())
}

/// Format an existing image file as ext4. `-F` forces mkfs to operate on a
/// regular file (not a block device); `-q` silences the banner.
async fn run_mkfs_ext4(path: &Path) -> Result<()> {
    let output = tokio::process::Command::new("mkfs.ext4")
        .arg("-F")
        .arg("-q")
        .arg(path)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::Internal(
                    "mkfs.ext4 not found; ext4 volume images require Linux (e2fsprogs)".to_string(),
                )
            } else {
                ApiError::Internal(format!("spawn mkfs.ext4: {e}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ApiError::Internal(format!(
            "mkfs.ext4 failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct VolumeEntry {
    id: String,
    name: String,
    size_mb: u32,
    created_at: SystemTime,
    mount_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Manages creation, lookup, and removal of named persistent volumes.
pub struct VolumeManager {
    data_dir: PathBuf,
    volumes: Arc<RwLock<HashMap<String, VolumeEntry>>>,
    /// Maximum number of volumes. Prevents metadata and inode exhaustion.
    max_volumes: usize,
    formatter: Arc<dyn VolumeFormatter>,
}

impl VolumeManager {
    /// Production constructor: formats real ext4 images.
    pub fn new(data_dir: PathBuf, max_volumes: usize) -> Self {
        Self::with_formatter(data_dir, max_volumes, Arc::new(Ext4Formatter))
    }

    /// Construct with an injected formatter. Tests pass a no-op so they
    /// don't depend on `mkfs.ext4`.
    pub fn with_formatter(
        data_dir: PathBuf,
        max_volumes: usize,
        formatter: Arc<dyn VolumeFormatter>,
    ) -> Self {
        Self {
            data_dir,
            volumes: Arc::new(RwLock::new(HashMap::new())),
            max_volumes,
            formatter,
        }
    }

    // -----------------------------------------------------------------------
    // CRUD
    // -----------------------------------------------------------------------

    /// Create a new volume, allocating backing storage on disk.
    pub async fn create(&self, req: CreateVolumeRequest) -> Result<PbVolumeInfo> {
        crate::validate::volume_name(&req.name)?;

        // A volume needs a concrete size to allocate its backing image.
        if req.size_mb == 0 {
            return Err(ApiError::InvalidRequest(
                "volume size_mb must be greater than 0".to_string(),
            ));
        }

        // Enforce volume cap to prevent resource exhaustion.
        let current = self.volumes.read().await.len();
        if current >= self.max_volumes {
            return Err(ApiError::InvalidRequest(format!(
                "volume limit reached ({}/{})",
                current, self.max_volumes,
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let mount_path = self.data_dir.join("volumes").join(&id);

        tokio::fs::create_dir_all(&mount_path)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        // Allocate the fixed-size ext4 image the sandbox will mount. The
        // image lives inside the volume's directory and is attached to a
        // microVM via krun_add_disk2 (see the mounts/attach work).
        let image_path = mount_path.join(VOLUME_IMAGE);
        self.formatter.format(&image_path, req.size_mb).await?;

        let entry = VolumeEntry {
            id: id.clone(),
            name: req.name.clone(),
            size_mb: req.size_mb,
            created_at: SystemTime::now(),
            mount_path: mount_path.clone(),
        };

        self.volumes.write().await.insert(id, entry.clone());
        Ok(entry_to_pb(entry))
    }

    /// Retrieve info for a volume by ID.
    pub async fn get(&self, id: &str) -> Result<PbVolumeInfo> {
        crate::validate::entity_id(id, "volume")?;
        self.volumes
            .read()
            .await
            .get(id)
            .cloned()
            .map(entry_to_pb)
            .ok_or_else(|| ApiError::VolumeNotFound(id.to_string()))
    }

    /// List all volumes.
    pub async fn list(&self) -> Result<Vec<PbVolumeInfo>> {
        Ok(self
            .volumes
            .read()
            .await
            .values()
            .cloned()
            .map(entry_to_pb)
            .collect())
    }

    /// Remove a volume, deleting its backing storage.
    pub async fn remove(&self, id: &str) -> Result<()> {
        crate::validate::entity_id(id, "volume")?;
        let entry = self
            .volumes
            .write()
            .await
            .remove(id)
            .ok_or_else(|| ApiError::VolumeNotFound(id.to_string()))?;

        if entry.mount_path.exists() {
            tokio::fs::remove_dir_all(&entry.mount_path)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Conversion helper
// ---------------------------------------------------------------------------

fn entry_to_pb(e: VolumeEntry) -> PbVolumeInfo {
    let d = e
        .created_at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    PbVolumeInfo {
        id: e.id,
        name: e.name,
        size_mb: e.size_mb,
        created_at: Some(prost_types::Timestamp {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        }),
        mount_path: e.mount_path.to_string_lossy().into_owned(),
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// Style: BDD names (`given_X_when_Y_then_Z`) with AAA bodies. Each test
// builds its own VolumeManager pointed at a fresh tempdir so they run in
// parallel without interfering.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Test formatter: creates the image file at the requested size but
    /// skips `mkfs.ext4`, so the CRUD tests run offline and on any OS.
    #[derive(Debug)]
    struct FakeFormatter;

    #[async_trait::async_trait]
    impl VolumeFormatter for FakeFormatter {
        async fn format(&self, image_path: &Path, size_mb: u32) -> Result<()> {
            allocate_sparse_image(image_path, size_mb).await
        }
    }

    /// Build a VolumeManager with a per-test temp data dir.
    /// The TempDir is leaked intentionally: tokio's async fs API outlives
    /// any test-local scope and clean-up happens on process exit.
    fn build_manager(max_volumes: usize) -> VolumeManager {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        VolumeManager::with_formatter(path, max_volumes, Arc::new(FakeFormatter))
    }

    fn req(name: &str, size_mb: u32) -> CreateVolumeRequest {
        CreateVolumeRequest {
            name: name.to_string(),
            size_mb,
        }
    }

    // ----- create --------------------------------------------------------

    #[tokio::test]
    async fn given_empty_manager_when_create_volume_then_returns_info_with_uuid() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let v = mgr.create(req("demo", 256)).await.expect("create");

        // Assert: a UUID was assigned (36 chars with hyphens) and the
        // requested fields are echoed back verbatim.
        assert_eq!(v.id.len(), 36);
        assert_eq!(v.name, "demo");
        assert_eq!(v.size_mb, 256);
        assert!(v.created_at.is_some());
    }

    #[tokio::test]
    async fn given_empty_manager_when_create_volume_then_mount_path_exists_on_disk() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let v = mgr.create(req("demo", 256)).await.expect("create");

        // Assert: backing directory was actually created. The manager
        // promises mount_path is usable immediately after create returns.
        assert!(
            std::path::Path::new(&v.mount_path).exists(),
            "mount_path {} does not exist",
            v.mount_path
        );
    }

    #[tokio::test]
    async fn given_invalid_name_when_create_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .create(req("bad name with spaces", 256))
            .await
            .expect_err("must reject invalid name");

        // Assert: validation maps to InvalidRequest, not Internal.
        // The gRPC layer relies on this variant to produce InvalidArgument.
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn given_manager_at_capacity_when_create_then_returns_invalid_request() {
        // Arrange: fill the manager exactly to capacity.
        let mgr = build_manager(2);
        mgr.create(req("v1", 100)).await.unwrap();
        mgr.create(req("v2", 100)).await.unwrap();

        // Act: one more triggers the cap.
        let err = mgr.create(req("v3", 100)).await.expect_err("cap");

        // Assert: cap surfaces as InvalidRequest so the gRPC client sees
        // InvalidArgument with a helpful message. The exact message text
        // is asserted to mention "limit" so users can grep for it.
        match err {
            ApiError::InvalidRequest(msg) => {
                assert!(msg.contains("limit"), "message lacks 'limit': {msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    // ----- get -----------------------------------------------------------

    #[tokio::test]
    async fn given_existing_volume_when_get_then_returns_same_info() {
        // Arrange
        let mgr = build_manager(4);
        let created = mgr.create(req("demo", 256)).await.unwrap();

        // Act
        let fetched = mgr.get(&created.id).await.expect("get");

        // Assert: the get response equals the create response on every
        // field that should round-trip. Timestamps are compared on seconds
        // only since SystemTime conversion is lossy at the nanosecond.
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, created.name);
        assert_eq!(fetched.size_mb, created.size_mb);
        assert_eq!(fetched.mount_path, created.mount_path);
    }

    #[tokio::test]
    async fn given_unknown_id_when_get_then_returns_volume_not_found() {
        // Arrange: well-formed but unknown ID. Validator passes it; lookup
        // fails. This is the explicit boundary between InvalidRequest and
        // NotFound the gRPC layer maps to InvalidArgument vs NotFound.
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .get("00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown id must error");

        // Assert
        assert!(matches!(err, ApiError::VolumeNotFound(_)));
    }

    #[tokio::test]
    async fn given_malformed_id_when_get_then_returns_invalid_request() {
        // Arrange
        let mgr = build_manager(4);

        // Act: non-hex characters fail validate::entity_id before lookup.
        let err = mgr
            .get("not-a-valid-uuid-zzzz")
            .await
            .expect_err("malformed id");

        // Assert
        assert!(matches!(err, ApiError::InvalidRequest(_)));
    }

    // ----- list ----------------------------------------------------------

    #[tokio::test]
    async fn given_empty_manager_when_list_then_returns_empty_vec() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let volumes = mgr.list().await.expect("list");

        // Assert
        assert!(volumes.is_empty());
    }

    #[tokio::test]
    async fn given_three_volumes_when_list_then_returns_all_three() {
        // Arrange
        let mgr = build_manager(4);
        mgr.create(req("a", 100)).await.unwrap();
        mgr.create(req("b", 200)).await.unwrap();
        mgr.create(req("c", 300)).await.unwrap();

        // Act
        let mut volumes = mgr.list().await.expect("list");

        // Assert: every name appears. Order is unspecified (HashMap), so
        // we sort before comparing to avoid flakes.
        volumes.sort_by(|x, y| x.name.cmp(&y.name));
        let names: Vec<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    // ----- remove --------------------------------------------------------

    #[tokio::test]
    async fn given_existing_volume_when_remove_then_get_returns_not_found() {
        // Arrange
        let mgr = build_manager(4);
        let v = mgr.create(req("demo", 256)).await.unwrap();

        // Act
        mgr.remove(&v.id).await.expect("remove");

        // Assert: subsequent get on the removed ID returns NotFound, not
        // a stale entry.
        let err = mgr.get(&v.id).await.expect_err("must be gone");
        assert!(matches!(err, ApiError::VolumeNotFound(_)));
    }

    #[tokio::test]
    async fn given_existing_volume_when_remove_then_mount_path_is_deleted() {
        // Arrange
        let mgr = build_manager(4);
        let v = mgr.create(req("demo", 256)).await.unwrap();
        assert!(std::path::Path::new(&v.mount_path).exists());

        // Act
        mgr.remove(&v.id).await.expect("remove");

        // Assert: directory is cleaned up. Leaving stale directories around
        // would leak disk over time.
        assert!(
            !std::path::Path::new(&v.mount_path).exists(),
            "mount path still exists after remove: {}",
            v.mount_path
        );
    }

    #[tokio::test]
    async fn given_unknown_id_when_remove_then_returns_volume_not_found() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let err = mgr
            .remove("00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("unknown id");

        // Assert
        assert!(matches!(err, ApiError::VolumeNotFound(_)));
    }

    #[tokio::test]
    async fn given_volume_removed_when_create_then_cap_slot_is_freed() {
        // Arrange: regression — removing a volume must release its slot
        // in the cap counter, otherwise a manager could lock up over time.
        let mgr = build_manager(2);
        let v1 = mgr.create(req("v1", 100)).await.unwrap();
        let _v2 = mgr.create(req("v2", 100)).await.unwrap();
        // Manager is now at cap.

        // Act: remove one, then create another.
        mgr.remove(&v1.id).await.unwrap();
        let v3 = mgr.create(req("v3", 100)).await;

        // Assert
        assert!(v3.is_ok(), "removing a volume must free a slot");
    }

    // ----- backing image -------------------------------------------------

    #[tokio::test]
    async fn given_zero_size_when_create_then_returns_invalid_request() {
        // Arrange: a sizeless volume can't be allocated.
        let mgr = build_manager(4);

        // Act
        let err = mgr.create(req("demo", 0)).await.expect_err("zero size");

        // Assert: rejected before any disk work, as InvalidRequest.
        match err {
            ApiError::InvalidRequest(msg) => assert!(msg.contains("size_mb"), "got: {msg}"),
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn given_create_when_succeeds_then_image_allocated_at_requested_size() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let v = mgr.create(req("demo", 8)).await.expect("create");

        // Assert: the backing image exists inside the volume dir and reports
        // the requested size (sparse, so on-disk usage may be less).
        let image = std::path::Path::new(&v.mount_path).join(VOLUME_IMAGE);
        let meta = std::fs::metadata(&image).expect("image metadata");
        assert_eq!(meta.len(), 8 * 1024 * 1024);
    }

    #[tokio::test]
    async fn given_allocate_sparse_image_when_called_then_file_has_exact_length() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("v.img");

        // Act
        allocate_sparse_image(&path, 4).await.expect("allocate");

        // Assert
        let meta = std::fs::metadata(&path).expect("metadata");
        assert_eq!(meta.len(), 4 * 1024 * 1024);
    }

    /// Real `mkfs.ext4` path. Skips cleanly where e2fsprogs is absent (dev
    /// macOS), so it's a no-op locally and exercises the real tool on Linux
    /// CI.
    #[tokio::test]
    async fn given_ext4_formatter_when_format_then_produces_ext4_image() {
        // Arrange: only meaningful where mkfs.ext4 exists.
        let probe = tokio::process::Command::new("mkfs.ext4")
            .arg("-V")
            .output()
            .await;
        if probe.is_err() {
            eprintln!("mkfs.ext4 unavailable — skipping real-format test");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let image = dir.path().join("real.img");

        // Act
        Ext4Formatter.format(&image, 8).await.expect("format ext4");

        // Assert: ext4 superblock magic 0xEF53 sits at offset 0x438
        // (little-endian: bytes 0x53, 0xEF).
        let bytes = std::fs::read(&image).expect("read image");
        assert_eq!(&bytes[0x438..0x43A], &[0x53, 0xEF], "missing ext4 magic");
    }
}
