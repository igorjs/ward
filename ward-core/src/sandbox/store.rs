// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! On-disk sandbox records: one `meta.json` per sandbox directory.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::{CreateOpts, SandboxStatus};

/// Directory under the data dir that holds one subdirectory per sandbox.
const SANDBOXES_DIR: &str = "sandboxes";
const META_FILE: &str = "meta.json";
const META_TMP_FILE: &str = "meta.json.tmp";
const ROOTFS_DIR: &str = "rootfs";
/// Sandbox dir mode (rwx------): the record holds env values.
const DIR_MODE: u32 = 0o700;
/// Record file mode (rw-------).
const FILE_MODE: u32 = 0o600;

/// What the daemon remembers about a sandbox across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxMeta {
    pub id: String,
    /// Create options, including name, main command, tty and workdir.
    pub options: CreateOpts,
    pub created_at: SystemTime,
    pub status: SandboxStatus,
    pub exit_code: Option<i32>,
    /// Hex SHA-256 of the capability token. The token itself is never stored.
    pub token_hash: String,
}

/// Hex SHA-256 of a capability token.
pub fn hash_token(token: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Constant-time equality of two token hashes. The only place token hashes
/// are compared.
pub fn token_hash_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Reads and writes sandbox records under `<data>/sandboxes`.
#[derive(Debug, Clone)]
pub struct MetaStore {
    root: PathBuf,
}

impl MetaStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join(SANDBOXES_DIR),
        }
    }

    pub fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn rootfs_path(&self, id: &str) -> PathBuf {
        self.sandbox_dir(id).join(ROOTFS_DIR)
    }

    /// Write a record atomically: temp file, fsync, rename, fsync of the dir.
    pub fn save(&self, meta: &SandboxMeta) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = self.sandbox_dir(&meta.id);
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(DIR_MODE))?;

        let tmp = dir.join(META_TMP_FILE);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .open(&tmp)?;
        // A leftover temp file keeps the mode it was created with.
        file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
        file.write_all(&serde_json::to_vec_pretty(meta).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, dir.join(META_FILE))?;
        std::fs::File::open(&dir)?.sync_all()
    }

    /// Delete a sandbox directory and everything in it. The record goes
    /// first and is made durable, so an interrupted delete never leaves a
    /// half-removed sandbox that reloads.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let dir = self.sandbox_dir(id);
        match std::fs::remove_file(dir.join(META_FILE)) {
            Ok(()) => std::fs::File::open(&dir)?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::remove_dir_all(dir)
    }

    /// Scan the sandbox dirs and return the records to adopt, after
    /// startup reconciliation. A dir whose record cannot be read is left
    /// alone, so an older daemon never destroys data it cannot parse.
    pub fn load_all(&self) -> Vec<SandboxMeta> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "cannot scan sandbox directory");
                }
                return Vec::new();
            }
        };
        entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| self.load_one(&entry.file_name().to_string_lossy()))
            .collect()
    }

    fn load_one(&self, id: &str) -> Option<SandboxMeta> {
        let dir = self.sandbox_dir(id);
        let _ = std::fs::remove_file(dir.join(META_TMP_FILE));

        let text = match std::fs::read_to_string(dir.join(META_FILE)) {
            Ok(text) => text,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound && !self.rootfs_path(id).exists() =>
            {
                tracing::info!(sandbox_id = %id, "removing sandbox dir with no record and no rootfs");
                if let Err(e) = self.delete(id) {
                    tracing::warn!(sandbox_id = %id, error = %e, "cannot remove orphan sandbox dir");
                }
                return None;
            }
            Err(e) => {
                tracing::warn!(sandbox_id = %id, error = %e, "skipping sandbox: record unreadable");
                return None;
            }
        };
        let mut meta: SandboxMeta = match serde_json::from_str(&text) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(sandbox_id = %id, error = %e, "skipping sandbox: record invalid");
                return None;
            }
        };
        if meta.id != id {
            tracing::warn!(sandbox_id = %id, "skipping sandbox: record id does not match its directory");
            return None;
        }

        let reconciled = match meta.status {
            SandboxStatus::Running => SandboxStatus::Stopped,
            SandboxStatus::Creating | SandboxStatus::Stopping => SandboxStatus::Failed,
            _ => return Some(meta),
        };
        meta.status = reconciled;
        if let Err(e) = self.save(&meta) {
            tracing::warn!(sandbox_id = %id, error = %e, "cannot write reconciled state");
        }
        Some(meta)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    use rstest::rstest;

    use super::*;
    use crate::protocol::SandboxSpec;

    const TOKEN: &str = "11111111-2222-3333-4444-555555555555";
    const ID_A: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const ID_B: &str = "bbbbbbbb-0000-0000-0000-000000000002";

    fn meta(id: &str, status: SandboxStatus) -> SandboxMeta {
        SandboxMeta {
            id: id.to_string(),
            options: CreateOpts {
                image: "alpine".into(),
                mounts: Vec::new(),
                volume_ids: Vec::new(),
                egress: Default::default(),
                resources: Default::default(),
                env: HashMap::from([("SECRET".to_string(), "v".to_string())]),
                from_snapshot: None,
                comms: Default::default(),
                spec: SandboxSpec {
                    name: "web1".into(),
                    command: vec!["sh".into(), "-c".into(), "echo hi".into()],
                    workdir: "/srv".into(),
                    tty: true,
                },
            },
            created_at: SystemTime::now(),
            status,
            exit_code: None,
            token_hash: hash_token(TOKEN),
        }
    }

    fn store() -> (tempfile::TempDir, MetaStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = MetaStore::new(tmp.path());
        (tmp, store)
    }

    fn meta_path(store: &MetaStore, id: &str) -> PathBuf {
        store.sandbox_dir(id).join(META_FILE)
    }

    #[test]
    fn given_running_sandbox_on_disk_when_loaded_then_stopped_with_same_id_name_and_options() {
        // Arrange
        let (_tmp, store) = store();
        let saved = meta(ID_A, SandboxStatus::Running);
        store.save(&saved).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, ID_A);
        assert_eq!(loaded[0].status, SandboxStatus::Stopped);
        assert_eq!(loaded[0].options.spec.name, "web1");
        assert_eq!(loaded[0].options.image, "alpine");
        assert_eq!(loaded[0].options.env, saved.options.env);
    }

    #[test]
    fn given_command_tty_and_workdir_when_loaded_then_unchanged() {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, SandboxStatus::Stopped)).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        let spec = &loaded[0].options.spec;
        assert_eq!(spec.command, vec!["sh", "-c", "echo hi"]);
        assert!(spec.tty);
        assert_eq!(spec.workdir, "/srv");
    }

    #[test]
    fn given_exited_sandbox_with_code_7_when_loaded_then_still_exited_with_7() {
        // Arrange
        let (_tmp, store) = store();
        let mut exited = meta(ID_A, SandboxStatus::Exited);
        exited.exit_code = Some(7);
        store.save(&exited).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert_eq!(loaded[0].status, SandboxStatus::Exited);
        assert_eq!(loaded[0].exit_code, Some(7));
    }

    #[rstest]
    #[case::creating(SandboxStatus::Creating)]
    #[case::stopping(SandboxStatus::Stopping)]
    fn given_transient_state_when_loaded_then_failed(#[case] state: SandboxStatus) {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, state)).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert_eq!(loaded[0].status, SandboxStatus::Failed);
    }

    #[test]
    fn given_reconciled_state_when_loaded_then_written_back() {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, SandboxStatus::Running)).unwrap();
        store.load_all();

        // Act
        let text = std::fs::read_to_string(meta_path(&store, ID_A)).unwrap();

        // Assert
        assert!(text.contains("\"stopped\""), "{text}");
    }

    #[rstest]
    #[case::invalid_json("{not json")]
    #[case::truncated("{\"id\":\"aaaaaaaa-0000")]
    #[case::missing_field("{\"id\":\"aaaaaaaa-0000-0000-0000-000000000001\"}")]
    #[case::empty("")]
    fn given_bad_meta_when_loaded_then_skipped_kept_and_others_load(#[case] body: &str) {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_B, SandboxStatus::Stopped)).unwrap();
        let bad_dir = store.sandbox_dir(ID_A);
        std::fs::create_dir_all(bad_dir.join(ROOTFS_DIR)).unwrap();
        std::fs::write(bad_dir.join(META_FILE), body).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, ID_B);
        assert!(bad_dir.join(ROOTFS_DIR).is_dir(), "dir must stay in place");
        assert!(bad_dir.join(META_FILE).is_file());
    }

    #[test]
    fn given_dir_without_meta_or_rootfs_when_loaded_then_removed() {
        // Arrange
        let (_tmp, store) = store();
        let orphan = store.sandbox_dir(ID_A);
        std::fs::create_dir_all(&orphan).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert!(loaded.is_empty());
        assert!(!orphan.exists());
    }

    #[test]
    fn given_dir_without_meta_but_with_rootfs_when_loaded_then_kept() {
        // Arrange
        let (_tmp, store) = store();
        std::fs::create_dir_all(store.rootfs_path(ID_A)).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert!(loaded.is_empty());
        assert!(store.rootfs_path(ID_A).is_dir());
    }

    #[test]
    fn given_undeletable_rootfs_when_deleted_then_meta_is_gone_and_dir_not_reloaded() {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, SandboxStatus::Stopped)).unwrap();
        let locked = store.rootfs_path(ID_A).join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("file"), b"").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        // Act
        let result = store.delete(ID_A);

        // Assert
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(DIR_MODE)).unwrap();
        assert!(result.is_err());
        assert!(!meta_path(&store, ID_A).exists());
        assert!(store.load_all().is_empty());
    }

    #[test]
    fn given_meta_whose_id_differs_from_dir_when_loaded_then_skipped() {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, SandboxStatus::Stopped)).unwrap();
        std::fs::rename(store.sandbox_dir(ID_A), store.sandbox_dir(ID_B)).unwrap();

        // Act
        let loaded = store.load_all();

        // Assert
        assert!(loaded.is_empty());
    }

    #[test]
    fn given_persisted_sandbox_when_inspected_then_file_is_0600() {
        // Arrange
        let (_tmp, store) = store();

        // Act
        store.save(&meta(ID_A, SandboxStatus::Running)).unwrap();

        // Assert
        let mode = std::fs::metadata(meta_path(&store, ID_A))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, FILE_MODE);
    }

    #[test]
    fn given_persisted_sandbox_when_inspected_then_dir_is_0700() {
        // Arrange
        let (_tmp, store) = store();

        // Act
        store.save(&meta(ID_A, SandboxStatus::Running)).unwrap();

        // Assert
        let mode = std::fs::metadata(store.sandbox_dir(ID_A))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, DIR_MODE);
    }

    #[test]
    fn given_saved_twice_when_inspected_then_no_temp_file_is_left() {
        // Arrange
        let (_tmp, store) = store();

        // Act
        store.save(&meta(ID_A, SandboxStatus::Running)).unwrap();
        store.save(&meta(ID_A, SandboxStatus::Stopped)).unwrap();

        // Assert
        assert!(!store.sandbox_dir(ID_A).join(META_TMP_FILE).exists());
    }

    #[test]
    fn given_sandbox_on_disk_when_meta_read_then_hash_present_and_token_absent() {
        // Arrange
        let (_tmp, store) = store();
        store.save(&meta(ID_A, SandboxStatus::Running)).unwrap();

        // Act
        let text = std::fs::read_to_string(meta_path(&store, ID_A)).unwrap();

        // Assert
        assert!(text.contains(&hash_token(TOKEN)));
        assert!(!text.contains(TOKEN));
    }

    #[test]
    fn given_equal_and_unequal_hashes_when_compared_then_helper_tells_them_apart() {
        // Arrange
        let a = hash_token("one");

        // Act / Assert
        assert!(token_hash_eq(&a, &hash_token("one")));
        assert!(!token_hash_eq(&a, &hash_token("two")));
        assert!(!token_hash_eq(&a, ""));
    }

    #[test]
    fn given_auth_sources_when_scanned_then_helper_uses_subtle_and_no_hash_is_compared_with_eq() {
        // Arrange
        let sources = [
            ("store.rs", include_str!("store.rs")),
            ("manager.rs", include_str!("manager.rs")),
        ];

        // Act / Assert
        for (name, src) in sources {
            let production = src.split("\n#[cfg(test)]").next().unwrap();
            let in_helper = production
                .split("pub fn token_hash_eq")
                .nth(1)
                .map(|rest| rest.split("\n}\n").next().unwrap_or(""))
                .unwrap_or("");
            if name == "store.rs" {
                assert!(
                    in_helper.contains("ConstantTimeEq"),
                    "helper must use subtle"
                );
            }
            for line in production.lines() {
                let compares = line.contains(" == ") || line.contains(" != ");
                if compares && line.to_lowercase().contains("hash") && !in_helper.contains(line) {
                    panic!("{name}: hash compared with ==/!=: {line}");
                }
            }
        }
    }
}
