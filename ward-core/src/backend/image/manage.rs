// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Removing, tagging and describing images already in the cache.

use std::collections::HashSet;
use std::path::PathBuf;

use super::{
    BackendError, CacheMeta, DIGEST_ALGORITHM_PREFIX, HistoryEntry, ImageStore, Result,
    validate_reference, write_meta_json,
};
use crate::sync::LockUnpoisoned;

/// Docker shows this many characters of an image or container id.
const SHORT_ID_LEN: usize = 12;

/// What `remove_image` took out of the cache, in the order Docker lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageRemoval {
    /// References that no longer name the image.
    pub untagged: Vec<String>,
    /// Ids of the images deleted.
    pub deleted: Vec<String>,
}

/// The build steps of one cached image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryView {
    pub image_id: String,
    /// Oldest first.
    pub entries: Vec<HistoryEntry>,
}

/// How a name given by the user matched a cache entry.
enum Matched {
    /// A reference of the image; the text is the reference as stored.
    Tag(String),
    /// The image id or a prefix of it.
    Id,
}

type Found = (PathBuf, CacheMeta, Matched);

fn short(id: &str) -> &str {
    id.get(..SHORT_ID_LEN).unwrap_or(id)
}

fn short_image_id(digest: &str) -> &str {
    short(
        digest
            .strip_prefix(DIGEST_ALGORITHM_PREFIX)
            .unwrap_or(digest),
    )
}

/// Whether `a` and `b` name the same image once registry and tag defaults
/// are filled in, so `alpine` and `docker.io/library/alpine:latest` match.
fn same_reference(a: &str, b: &str) -> bool {
    match (validate_reference(a), validate_reference(b)) {
        (Ok(a), Ok(b)) => a.whole() == b.whole(),
        _ => false,
    }
}

impl ImageStore {
    /// The cache entry `name` refers to: a reference, an image id or a
    /// unique prefix of one. A tag wins over an id prefix, as in Docker.
    fn find_named(&self, name: &str) -> Result<Option<Found>> {
        let entries = self.scan_cache_entries()?;
        if let Some((dir, meta)) = entries.iter().find(|(_, meta)| meta.has_ref(name)) {
            let matched = if meta.digest == name {
                Matched::Id
            } else {
                Matched::Tag(name.to_string())
            };
            return Ok(Some((dir.clone(), meta.clone(), matched)));
        }
        let tagged = entries.iter().find_map(|(dir, meta)| {
            meta.refs()
                .find(|stored| same_reference(stored, name))
                .map(|stored| (dir.clone(), meta.clone(), Matched::Tag(stored.to_string())))
        });
        if tagged.is_some() {
            return Ok(tagged);
        }
        let prefix = name
            .strip_prefix(DIGEST_ALGORITHM_PREFIX)
            .unwrap_or(name)
            .to_ascii_lowercase();
        if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let mut by_id = entries.into_iter().filter(|(_, meta)| {
            meta.digest
                .strip_prefix(DIGEST_ALGORITHM_PREFIX)
                .is_some_and(|hex| hex.starts_with(&prefix))
        });
        let first = by_id.next();
        if by_id.next().is_some() {
            return Err(BackendError::InvalidArgument(format!(
                "multiple IDs found with provided prefix: {prefix}"
            )));
        }
        Ok(first.map(|(dir, meta)| (dir, meta, Matched::Id)))
    }

    fn require_named(&self, name: &str) -> Result<Found> {
        self.find_named(name)?
            .ok_or_else(|| BackendError::ImageNotFound(format!("No such image: {name}")))
    }

    /// Remove `name` like `docker rmi`. A tag that is not the image's last
    /// reference is only untagged. Otherwise the image is deleted, which is
    /// refused while a sandbox in `running` uses it, and, without `force`,
    /// while any other sandbox does or while several tags name it and it was
    /// given by id. Per-sandbox rootfs copies are unaffected.
    pub fn remove_image(
        &self,
        name: &str,
        force: bool,
        running: &HashSet<String>,
    ) -> Result<ImageRemoval> {
        let _guard = self.install_lock.lock_unpoisoned();
        let (dir, meta, matched) = self.require_named(name)?;
        let id = short_image_id(&meta.digest);
        if let Matched::Tag(tag) = &matched {
            let rest = meta.without_ref(tag);
            if !rest.is_untagged() {
                write_meta_json(&dir.join("meta.json"), &rest)?;
                return Ok(ImageRemoval {
                    untagged: vec![tag.clone()],
                    deleted: Vec::new(),
                });
            }
        }
        let tags: Vec<String> = meta
            .refs()
            .filter(|reference| *reference != meta.digest)
            .map(String::from)
            .collect();
        if matches!(matched, Matched::Id) && tags.len() > 1 && !force {
            return Err(BackendError::InvalidArgument(format!(
                "conflict: unable to delete {id} (must be forced) - image is referenced in multiple repositories"
            )));
        }
        let users = self.sandboxes_using(&meta.digest);
        if let Some(sandbox) = users.iter().find(|sandbox| running.contains(*sandbox)) {
            return Err(BackendError::InvalidArgument(format!(
                "conflict: unable to delete {id} (cannot be forced) - image is being used by running container {}",
                short(sandbox)
            )));
        }
        if let (Some(sandbox), false) = (users.first(), force) {
            return Err(BackendError::InvalidArgument(format!(
                "conflict: unable to delete {id} (must be forced) - image is being used by stopped container {}",
                short(sandbox)
            )));
        }
        std::fs::remove_dir_all(&dir).map_err(BackendError::Io)?;
        Ok(ImageRemoval {
            untagged: tags,
            deleted: vec![meta.digest],
        })
    }

    /// Give the image `source` names the tag `target`, like `docker tag`. A
    /// tag another image holds moves here.
    pub fn tag_image(&self, source: &str, target: &str) -> Result<()> {
        validate_reference(target)?;
        let _guard = self.install_lock.lock_unpoisoned();
        let (dir, mut meta, _) = self.require_named(source)?;
        if meta.has_ref(target) {
            return Ok(());
        }
        self.untag_others(target, &meta.digest)?;
        meta.add_ref(target);
        write_meta_json(&dir.join("meta.json"), &meta)
    }

    /// The build steps of the image `name` refers to.
    pub fn image_history(&self, name: &str) -> Result<HistoryView> {
        let (_, meta, _) = self.require_named(name)?;
        Ok(HistoryView {
            image_id: meta.digest,
            entries: meta.config.map(|config| config.history).unwrap_or_default(),
        })
    }

    /// Ids of the sandboxes that hold the image with `digest`.
    fn sandboxes_using(&self, digest: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .holders
            .lock_unpoisoned()
            .iter()
            .filter(|(_, held)| held.as_str() == digest)
            .map(|(sandbox, _)| sandbox.clone())
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::backend::image::{ImagePuller, PulledImage};

    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl ImagePuller for FakePuller {
        async fn pull(&self, reference: &str, dest: &std::path::Path) -> Result<PulledImage> {
            std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
            let digest = format!("sha256:{}", "ab".repeat(32));
            let mut image = PulledImage::without_config(digest);
            image.config.history = vec![HistoryEntry {
                created_by: format!("pull {reference}"),
                ..HistoryEntry::default()
            }];
            Ok(image)
        }
    }

    async fn store_with(references: &[&str]) -> (tempfile::TempDir, ImageStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ImageStore::with_puller(dir.path().to_path_buf(), 64, Arc::new(FakePuller));
        for reference in references {
            store.pull(reference, None).await.expect("pull");
        }
        (dir, store)
    }

    fn none_running() -> HashSet<String> {
        HashSet::new()
    }

    #[tokio::test]
    async fn given_two_tags_when_one_is_removed_then_it_is_untagged_and_the_image_stays() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3", "alpine:latest"]).await;

        // Act
        let removal = store
            .remove_image("alpine:3", false, &none_running())
            .expect("remove");

        // Assert
        assert_eq!(removal.untagged, ["alpine:3"]);
        assert!(removal.deleted.is_empty());
        assert!(store.is_cached("alpine:latest").await);
    }

    #[tokio::test]
    async fn given_the_last_tag_when_removed_then_the_image_is_deleted() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;

        // Act
        let removal = store
            .remove_image("alpine:3", false, &none_running())
            .expect("remove");

        // Assert
        assert_eq!(removal.untagged, ["alpine:3"]);
        assert_eq!(removal.deleted, [format!("sha256:{}", "ab".repeat(32))]);
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn given_an_id_prefix_when_removed_then_the_image_is_found() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;

        // Act
        let removal = store
            .remove_image("abab", false, &none_running())
            .expect("remove");

        // Assert
        assert_eq!(removal.untagged, ["alpine:3"]);
    }

    #[tokio::test]
    async fn given_a_bare_name_when_removed_then_the_latest_tag_matches() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:latest"]).await;

        // Act
        let removal = store
            .remove_image("alpine", false, &none_running())
            .expect("remove");

        // Assert
        assert_eq!(removal.untagged, ["alpine:latest"]);
    }

    #[tokio::test]
    async fn given_a_missing_image_when_removed_then_it_is_not_found() {
        // Arrange
        let (_dir, store) = store_with(&[]).await;

        // Act
        let error = store
            .remove_image("nope", false, &none_running())
            .expect_err("missing");

        // Assert
        assert!(matches!(error, BackendError::ImageNotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn given_several_tags_when_removed_by_id_then_force_is_needed() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3", "alpine:latest"]).await;

        // Act
        let refused = store.remove_image("abab", false, &none_running());
        let forced = store
            .remove_image("abab", true, &none_running())
            .expect("forced");

        // Assert
        assert!(refused.is_err());
        assert_eq!(forced.untagged.len(), 2);
        assert_eq!(forced.deleted.len(), 1);
    }

    #[tokio::test]
    async fn given_sandboxes_using_the_image_when_removed_then_stopped_needs_force_and_running_never()
     {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;
        store.hold("sandbox-1", "alpine:3");
        let running: HashSet<String> = ["sandbox-1".to_string()].into();

        // Act
        let stopped_refused = store.remove_image("alpine:3", false, &none_running());
        let running_forced = store.remove_image("alpine:3", true, &running);
        let stopped_forced = store.remove_image("alpine:3", true, &none_running());

        // Assert
        let message = stopped_refused.expect_err("refused").to_string();
        assert!(message.contains("must be forced"), "{message}");
        let message = running_forced.expect_err("running").to_string();
        assert!(message.contains("cannot be forced"), "{message}");
        assert_eq!(stopped_forced.expect("forced").deleted.len(), 1);
    }

    #[tokio::test]
    async fn given_a_tag_held_by_another_image_when_tagged_then_it_moves() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;

        // Act
        store.tag_image("alpine:3", "mine:v1").expect("tag");
        store.tag_image("alpine:3", "mine:v1").expect("tag again");

        // Assert
        let mut references: Vec<String> = store
            .list()
            .await
            .into_iter()
            .map(|image| image.reference)
            .collect();
        references.sort();
        assert_eq!(references, ["alpine:3", "mine:v1"]);
    }

    #[tokio::test]
    async fn given_a_bad_target_when_tagged_then_it_is_refused() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;

        // Act
        let error = store.tag_image("alpine:3", "Not A Ref").expect_err("bad");

        // Assert
        assert!(matches!(error, BackendError::InvalidArgument(_)), "{error}");
    }

    #[tokio::test]
    async fn given_a_pulled_image_when_its_history_is_read_then_the_config_steps_are_returned() {
        // Arrange
        let (_dir, store) = store_with(&["alpine:3"]).await;

        // Act
        let view = store.image_history("alpine:3").expect("history");

        // Assert
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].created_by, "pull alpine:3");
    }

    #[test]
    fn given_config_history_when_layer_sizes_are_applied_then_empty_steps_get_none() {
        // Arrange
        let blob = br#"{"history":[
            {"created":"2024-01-02T03:04:05Z","created_by":"ADD rootfs","comment":"x"},
            {"created_by":"ENV A=1","empty_layer":true},
            {"created":"not a time","created_by":"RUN make"}]}"#;

        // Act
        let config = crate::backend::image::ImageConfig::from_blob(blob)
            .expect("parse")
            .with_layer_sizes(&[100, 200]);

        // Assert
        let sizes: Vec<u64> = config.history.iter().map(|e| e.size_bytes).collect();
        assert_eq!(sizes, [100, 0, 200]);
        assert!(config.history[0].created.is_some());
        assert!(config.history[2].created.is_none());
    }
}
