// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! VolumeManager: daemon-managed persistent volumes.
//!
//! A volume is a host directory under the data dir. A sandbox mounts it like
//! a bind mount, so the data outlives the sandbox and the daemon.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::AsyncWriteExt as _;
use tokio::sync::RwLock;

use crate::sync::LockUnpoisoned;

use crate::pb::{CreateVolumeRequest, VolumeInfo as PbVolumeInfo};
use crate::protocol::ApiError;

type Result<T> = std::result::Result<T, ApiError>;

/// Directory under the data dir that holds one directory per volume.
const VOLUMES_DIR: &str = "volumes";
/// Directory inside a volume's own directory that a sandbox sees.
const DATA_DIR: &str = "data";
/// Stands for a sandbox that is still being created in an in-use message.
const CREATING_HOLDER: &str = "a sandbox being created";
/// Owner-only modes: another local user must not read a volume.
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
/// File inside a volume's directory that records its identity.
const META_FILE: &str = "volume.json";
const META_TEMP_FILE: &str = "volume.json.tmp";
/// Size recorded for a volume that a sandbox creates on first use.
pub const IMPLICIT_VOLUME_SIZE_MB: u32 = 1024;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct VolumeEntry {
    id: String,
    name: String,
    size_mb: u32,
    created_at: SystemTime,
    /// The volume's own directory, which holds the metadata and the data.
    dir: PathBuf,
    labels: HashMap<String, String>,
}

impl VolumeEntry {
    fn data_path(&self) -> PathBuf {
        self.dir.join(DATA_DIR)
    }
}

/// What is written to `volume.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct VolumeMeta {
    id: String,
    name: String,
    size_mb: u32,
    created_at_secs: u64,
    #[serde(default)]
    labels: HashMap<String, String>,
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
    /// Volumes that a sandbox being created has named, with how many such
    /// sandboxes. They cannot be removed until the sandbox is recorded.
    claims: Claims,
}

type Claims = Arc<std::sync::Mutex<HashMap<String, usize>>>;

/// Holds a volume against removal while a sandbox that names it is being
/// created. Dropping it releases the hold.
pub struct VolumeClaim {
    claims: Claims,
    name: String,
}

impl Drop for VolumeClaim {
    fn drop(&mut self) {
        let mut claims = self.claims.lock_unpoisoned();
        if let Some(count) = claims.get_mut(&self.name) {
            *count -= 1;
            if *count == 0 {
                claims.remove(&self.name);
            }
        }
    }
}

impl VolumeManager {
    /// Open the volumes under `data_dir`, loading the ones a previous run
    /// left behind.
    pub fn new(data_dir: PathBuf, max_volumes: usize) -> Self {
        let volumes = load_volumes(&data_dir.join(VOLUMES_DIR));
        Self {
            data_dir,
            volumes: Arc::new(RwLock::new(volumes)),
            max_volumes,
            claims: Claims::default(),
        }
    }

    /// Hold the volume called `name` against removal, whether or not it
    /// exists yet.
    pub fn claim(&self, name: &str) -> VolumeClaim {
        *self
            .claims
            .lock_unpoisoned()
            .entry(name.to_string())
            .or_default() += 1;
        VolumeClaim {
            claims: Arc::clone(&self.claims),
            name: name.to_string(),
        }
    }

    fn is_claimed(&self, name: &str) -> bool {
        self.claims.lock_unpoisoned().contains_key(name)
    }

    // -----------------------------------------------------------------------
    // CRUD
    // -----------------------------------------------------------------------

    /// Create a volume. Creating a name that exists returns that volume, as
    /// Docker does.
    pub async fn create(&self, req: CreateVolumeRequest) -> Result<PbVolumeInfo> {
        crate::validate::volume_name(&req.name)?;
        crate::validate::volume_size(req.size_mb)?;
        crate::validate::labels(&req.labels)?;
        self.create_if_missing(&req.name, req.size_mb, req.labels)
            .await
    }

    /// The volume called `name`, created with the default size when missing.
    pub async fn ensure(&self, name: &str) -> Result<PbVolumeInfo> {
        crate::validate::volume_name(name)?;
        self.create_if_missing(name, IMPLICIT_VOLUME_SIZE_MB, HashMap::new())
            .await
    }

    async fn create_if_missing(
        &self,
        name: &str,
        size_mb: u32,
        labels: HashMap<String, String>,
    ) -> Result<PbVolumeInfo> {
        // The write lock spans the check and the insert, so two sandboxes
        // that name a new volume at once get the same one.
        let mut volumes = self.volumes.write().await;
        if let Some(existing) = volumes.values().find(|v| v.name == name) {
            return Ok(entry_to_pb(existing.clone()));
        }
        // Enforce volume cap to prevent resource exhaustion.
        if volumes.len() >= self.max_volumes {
            return Err(ApiError::InvalidRequest(format!(
                "volume limit reached ({}/{})",
                volumes.len(),
                self.max_volumes,
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let entry = VolumeEntry {
            id: id.clone(),
            name: name.to_string(),
            size_mb,
            created_at: SystemTime::now(),
            dir: self.data_dir.join(VOLUMES_DIR).join(&id),
            labels,
        };
        write_volume(&entry).await?;
        volumes.insert(id, entry.clone());
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

    /// Remove a volume that nothing uses. `users` lists the sandboxes that
    /// mount the volume; it runs under the manager's lock, so a sandbox
    /// cannot claim the volume between the check and the removal.
    pub async fn remove_if_unused<F, Fut>(&self, id: &str, users: F) -> Result<()>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Vec<String>>,
    {
        crate::validate::entity_id(id, "volume")?;
        let mut volumes = self.volumes.write().await;
        let entry = volumes
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::VolumeNotFound(id.to_string()))?;
        let mut holders = users(entry.name.clone()).await;
        if self.is_claimed(&entry.name) {
            holders.push(CREATING_HOLDER.to_string());
        }
        if !holders.is_empty() {
            return Err(ApiError::VolumeInUse(format!(
                "remove {}: volume is in use - [{}]",
                entry.name,
                holders.join(", ")
            )));
        }
        // Without its metadata the volume no longer loads, so a failed
        // delete below cannot bring back a half-removed volume on restart.
        match tokio::fs::remove_file(entry.dir.join(META_FILE)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ApiError::from_io("remove volume metadata", &e)),
        }
        volumes.remove(id);
        drop(volumes);
        tokio::fs::remove_dir_all(&entry.dir)
            .await
            .map_err(|e| ApiError::from_io("remove volume directory", &e))
    }

    /// Remove a volume, deleting its data.
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.remove_if_unused(id, |_| async { Vec::new() }).await
    }

    /// Remove the volume called `name` if nothing uses it. Returns whether
    /// it was removed.
    pub async fn remove_unused_by_name<F, Fut>(&self, name: &str, users: F) -> Result<bool>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Vec<String>>,
    {
        let id = self
            .volumes
            .read()
            .await
            .values()
            .find(|v| v.name == name)
            .map(|v| v.id.clone());
        let Some(id) = id else {
            return Ok(false);
        };
        match self.remove_if_unused(&id, users).await {
            Ok(()) => Ok(true),
            Err(ApiError::VolumeInUse(_) | ApiError::VolumeNotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Create the volume's directories and record its identity. The record is
/// written to a temporary file first, so a crash leaves no truncated one.
async fn write_volume(entry: &VolumeEntry) -> Result<()> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(entry.data_path())
        .await
        .map_err(|e| ApiError::from_io("create volume directory", &e))?;
    let meta = VolumeMeta {
        id: entry.id.clone(),
        name: entry.name.clone(),
        size_mb: entry.size_mb,
        created_at_secs: entry
            .created_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        labels: entry.labels.clone(),
    };
    let json = serde_json::to_vec(&meta)
        .map_err(|e| ApiError::Internal(format!("encode volume metadata: {e}")))?;
    let write_err = |e: std::io::Error| ApiError::from_io("write volume metadata", &e);
    let temp = entry.dir.join(META_TEMP_FILE);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(&temp)
        .await
        .map_err(write_err)?;
    file.write_all(&json).await.map_err(write_err)?;
    file.sync_all().await.map_err(write_err)?;
    tokio::fs::rename(&temp, entry.dir.join(META_FILE))
        .await
        .map_err(write_err)
}

/// Read every volume directory under `root`. A directory whose metadata is
/// missing, unreadable, inconsistent with its directory name, or a duplicate
/// of an earlier name is skipped, with a warning.
fn load_volumes(root: &Path) -> HashMap<String, VolumeEntry> {
    let Ok(dirs) = std::fs::read_dir(root) else {
        return HashMap::new();
    };
    let mut loaded: HashMap<String, VolumeEntry> = HashMap::new();
    let mut paths: Vec<PathBuf> = dirs.flatten().map(|d| d.path()).collect();
    paths.sort();
    for dir in paths {
        let entry = match read_entry(&dir) {
            Ok(entry) => entry,
            Err(reason) => {
                tracing::warn!(dir = %dir.display(), %reason, "skipping a volume directory");
                continue;
            }
        };
        if loaded.values().any(|v| v.name == entry.name) {
            tracing::warn!(dir = %dir.display(), name = %entry.name, "skipping a duplicate volume name");
            continue;
        }
        loaded.insert(entry.id.clone(), entry);
    }
    loaded
}

fn read_entry(dir: &Path) -> std::result::Result<VolumeEntry, String> {
    let bytes = std::fs::read(dir.join(META_FILE)).map_err(|e| format!("no metadata: {e}"))?;
    let meta: VolumeMeta =
        serde_json::from_slice(&bytes).map_err(|e| format!("unreadable metadata: {e}"))?;
    let dir_name = dir.file_name().and_then(|n| n.to_str());
    if dir_name != Some(meta.id.as_str()) {
        return Err("the id does not match the directory name".to_string());
    }
    crate::validate::volume_name(&meta.name).map_err(|e| e.to_string())?;
    Ok(VolumeEntry {
        id: meta.id,
        name: meta.name,
        size_mb: meta.size_mb,
        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(meta.created_at_secs),
        dir: dir.to_path_buf(),
        labels: meta.labels,
    })
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
        mount_path: e.data_path().to_string_lossy().into_owned(),
        id: e.id,
        name: e.name,
        size_mb: e.size_mb,
        created_at: Some(prost_types::Timestamp {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        }),
        labels: e.labels,
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

    /// Build a VolumeManager with a per-test temp data dir.
    /// The TempDir is leaked intentionally: tokio's async fs API outlives
    /// any test-local scope and clean-up happens on process exit.
    fn build_manager(max_volumes: usize) -> VolumeManager {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        VolumeManager::new(path, max_volumes)
    }

    fn req(name: &str, size_mb: u32) -> CreateVolumeRequest {
        CreateVolumeRequest {
            name: name.to_string(),
            size_mb,
            ..Default::default()
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
        // Arrange: regression: removing a volume must release its slot
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

    // ----- named volumes -------------------------------------------------

    #[tokio::test]
    async fn given_existing_name_when_create_then_returns_the_same_volume() {
        // Arrange
        let mgr = build_manager(4);
        let first = mgr.create(req("demo", 8)).await.unwrap();

        // Act
        let second = mgr.create(req("demo", 16)).await.unwrap();

        // Assert
        assert_eq!(second.id, first.id);
        assert_eq!(mgr.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn given_missing_name_when_ensure_then_volume_is_created_with_data_dir() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let v = mgr.ensure("cache").await.unwrap();

        // Assert
        assert_eq!(v.name, "cache");
        assert_eq!(v.size_mb, IMPLICIT_VOLUME_SIZE_MB);
        assert!(Path::new(&v.mount_path).is_dir());
        assert!(v.mount_path.ends_with(DATA_DIR));
    }

    #[tokio::test]
    async fn given_ensure_called_twice_when_same_name_then_one_volume_exists() {
        // Arrange
        let mgr = build_manager(4);

        // Act
        let a = mgr.ensure("cache").await.unwrap();
        let b = mgr.ensure("cache").await.unwrap();

        // Assert
        assert_eq!(a.id, b.id);
    }

    #[tokio::test]
    async fn given_volume_with_data_when_manager_reopens_then_volume_is_listed() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let first = VolumeManager::new(dir.path().to_path_buf(), 4);
        let v = first.ensure("keep").await.unwrap();
        std::fs::write(Path::new(&v.mount_path).join("f"), b"x").unwrap();

        // Act
        let second = VolumeManager::new(dir.path().to_path_buf(), 4);
        let listed = second.list().await.unwrap();

        // Assert
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, v.id);
        assert_eq!(listed[0].name, "keep");
        assert!(Path::new(&listed[0].mount_path).join("f").exists());
    }

    #[tokio::test]
    async fn given_a_claimed_volume_when_removed_then_it_is_in_use_until_the_claim_drops() {
        // Arrange
        let mgr = build_manager(4);
        let v = mgr.ensure("busy").await.unwrap();
        let claim = mgr.claim("busy");

        // Act
        let while_claimed = mgr.remove(&v.id).await;
        drop(claim);
        let after = mgr.remove(&v.id).await;

        // Assert
        assert!(matches!(while_claimed, Err(ApiError::VolumeInUse(_))));
        assert!(after.is_ok());
    }

    #[tokio::test]
    async fn given_a_sandbox_using_the_volume_when_removed_then_the_error_lists_it() {
        // Arrange
        let mgr = build_manager(4);
        let v = mgr.ensure("busy").await.unwrap();

        // Act
        let err = mgr
            .remove_if_unused(&v.id, |_| async { vec!["sandbox-1".to_string()] })
            .await
            .unwrap_err();

        // Assert
        assert_eq!(
            err.to_string(),
            "remove busy: volume is in use - [sandbox-1]"
        );
        assert!(mgr.get(&v.id).await.is_ok());
    }

    #[test]
    fn given_a_volume_directory_with_a_mismatched_id_when_loaded_then_it_is_skipped() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(VOLUMES_DIR).join("not-the-id");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = r#"{"id":"other","name":"x","size_mb":1,"created_at_secs":0}"#;
        std::fs::write(dir.join(META_FILE), meta).unwrap();

        // Act
        let loaded = load_volumes(&root.path().join(VOLUMES_DIR));

        // Assert
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn given_a_new_volume_when_created_then_its_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        // Arrange
        let mgr = build_manager(4);

        // Act
        let v = mgr.ensure("private").await.unwrap();

        // Assert
        let data_mode = std::fs::metadata(&v.mount_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(data_mode & 0o777, DIR_MODE);
    }

    #[tokio::test]
    async fn given_labels_when_volume_created_then_they_are_kept_and_listed() {
        // Arrange
        let mgr = build_manager(4);
        let mut request = req("tagged", 10);
        request.labels = [("env".to_string(), "dev".to_string())].into();

        // Act
        let created = mgr.create(request).await.expect("create");
        let listed = mgr.list().await.expect("list");

        // Assert
        assert_eq!(created.labels["env"], "dev");
        assert_eq!(listed[0].labels, created.labels);
    }

    #[tokio::test]
    async fn given_an_empty_label_key_when_volume_created_then_it_is_refused() {
        // Arrange
        let mgr = build_manager(4);
        let mut request = req("tagged", 10);
        request.labels = [(String::new(), "x".to_string())].into();

        // Act
        let result = mgr.create(request).await;

        // Assert
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn given_a_labelled_volume_when_the_manager_reopens_then_the_labels_are_loaded() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let first = VolumeManager::new(dir.path().to_path_buf(), 4);
        let mut request = req("tagged", 10);
        request.labels = [("env".to_string(), "dev".to_string())].into();
        first.create(request).await.expect("create");

        // Act
        let second = VolumeManager::new(dir.path().to_path_buf(), 4);
        let listed = second.list().await.expect("list");

        // Assert
        assert_eq!(listed[0].labels["env"], "dev");
    }
}
