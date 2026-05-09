// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! VolumeManager: daemon-managed shared persistent volumes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::pb::{CreateVolumeRequest, VolumeInfo as PbVolumeInfo};
use crate::protocol::ApiError;

type Result<T> = std::result::Result<T, ApiError>;

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
}

impl VolumeManager {
    pub fn new(data_dir: PathBuf, max_volumes: usize) -> Self {
        Self {
            data_dir,
            volumes: Arc::new(RwLock::new(HashMap::new())),
            max_volumes,
        }
    }

    // -----------------------------------------------------------------------
    // CRUD
    // -----------------------------------------------------------------------

    /// Create a new volume, allocating backing storage on disk.
    pub async fn create(&self, req: CreateVolumeRequest) -> Result<PbVolumeInfo> {
        crate::validate::volume_name(&req.name)?;

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

        // TODO: allocate a fixed-size filesystem image (e.g. ext4 via truncate + mkfs).

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
