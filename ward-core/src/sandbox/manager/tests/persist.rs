// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Record writes are serialised per sandbox and never hold the map lock.

use pretty_assertions::assert_eq;

use super::*;

const NAME: &str = "web1";
const RENAMED: &str = "web2";
const MAX_MANAGER_SANDBOXES: usize = 4;
const SHORT: std::time::Duration = std::time::Duration::from_millis(200);
const LONG: std::time::Duration = std::time::Duration::from_secs(30);

fn record_dir(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join("sandboxes").join(id)
}

#[tokio::test]
async fn given_a_write_in_flight_when_other_sandboxes_are_read_then_the_map_is_not_blocked() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let teardown = mgr.teardown();
    let held = teardown.lock_persist(&created.id).await;
    let writer = Arc::clone(&mgr);
    let rename = tokio::spawn(async move { writer.rename(NAME, RENAMED).await });

    // Act
    let listed = tokio::time::timeout(LONG, mgr.list()).await;
    let early = tokio::time::timeout(SHORT, async {
        while !rename.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await;

    // Assert
    assert!(listed.is_ok(), "list waited for a record write");
    assert!(
        early.is_err(),
        "rename wrote while another writer held the sandbox"
    );
    drop(held);
    let renamed = rename.await.unwrap().unwrap();
    assert_eq!(renamed.name, RENAMED);
}

#[tokio::test]
async fn given_a_write_in_flight_when_the_sandbox_is_removed_then_the_record_is_not_recreated() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let teardown = mgr.teardown();
    let held = teardown.lock_persist(&created.id).await;
    let remover = Arc::clone(&mgr);
    let id = created.id.clone();
    let remove = tokio::spawn(async move { remover.remove(&id).await });
    let writer = Arc::clone(&mgr);
    let id = created.id.clone();
    let write =
        tokio::spawn(async move { writer.record_state(&id, ProtoStatus::Running, None).await });
    tokio::time::sleep(SHORT).await;

    // Act
    drop(held);
    remove.await.unwrap().unwrap();
    let wrote = write.await.unwrap();

    // Assert
    assert!(wrote.is_ok() || matches!(wrote, Err(ApiError::SandboxNotFound(_))));
    assert!(!record_dir(tmp.path(), &created.id).exists());
}

#[tokio::test]
async fn given_a_failed_write_when_a_state_change_is_recorded_then_memory_keeps_the_old_state() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let dir = record_dir(tmp.path(), &created.id);
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();

    // Act
    let result = mgr
        .record_state(&created.id, ProtoStatus::Stopped, None)
        .await;

    // Assert
    assert!(result.is_err());
    let status = mgr
        .entries
        .read()
        .await
        .get(&created.id)
        .unwrap()
        .meta
        .status
        .clone();
    assert_eq!(status, ProtoStatus::Running);
}

#[tokio::test]
async fn given_concurrent_writers_when_all_finish_then_disk_matches_memory() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let statuses = [
        ProtoStatus::Stopped,
        ProtoStatus::Exited,
        ProtoStatus::Stopped,
        ProtoStatus::Exited,
    ];

    // Act
    let writers: Vec<_> = statuses
        .into_iter()
        .map(|status| {
            let (mgr, id) = (Arc::clone(&mgr), created.id.clone());
            tokio::spawn(async move { mgr.record_state(&id, status, None).await })
        })
        .collect();
    for writer in writers {
        writer.await.unwrap().unwrap();
    }

    // Assert
    let in_memory = mgr
        .entries
        .read()
        .await
        .get(&created.id)
        .unwrap()
        .meta
        .status
        .clone();
    let on_disk = crate::sandbox::store::MetaStore::new(tmp.path())
        .load_all()
        .remove(0)
        .status;
    assert_eq!(on_disk, in_memory);
}
