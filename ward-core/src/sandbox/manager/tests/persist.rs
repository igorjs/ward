// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Record writes are serialised per sandbox and never hold the map lock.
//! A gate holds one save mid-write, so each test sees the writer past its
//! plan and before it adopts.

use pretty_assertions::assert_eq;

use super::*;
use crate::sandbox::store::gate;

const NAME: &str = "web1";
const RENAMED: &str = "web2";
const MAX_MANAGER_SANDBOXES: usize = 4;
/// How long a task that must stay blocked gets to prove it does.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(100);
const LONG: std::time::Duration = std::time::Duration::from_secs(30);

fn record_dir(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join("sandboxes").join(id)
}

fn disk_status(root: &std::path::Path) -> ProtoStatus {
    crate::sandbox::store::MetaStore::new(root)
        .load_all()
        .remove(0)
        .status
}

async fn memory_status(mgr: &SandboxManager, id: &str) -> ProtoStatus {
    mgr.entries
        .read()
        .await
        .get(id)
        .unwrap()
        .meta
        .status
        .clone()
}

async fn until_removed_from_map(mgr: &SandboxManager, id: &str) {
    while mgr.entries.read().await.contains_key(id) {
        tokio::task::yield_now().await;
    }
}

/// Fails if the task finishes within `SETTLE`.
async fn assert_blocked<T>(task: &tokio::task::JoinHandle<T>, what: &str) {
    tokio::time::sleep(SETTLE).await;
    assert!(!task.is_finished(), "{what} did not wait");
}

fn spawn_record(
    mgr: &Arc<SandboxManager>,
    id: &str,
    status: ProtoStatus,
) -> tokio::task::JoinHandle<Result<()>> {
    let (mgr, id) = (Arc::clone(mgr), id.to_string());
    tokio::spawn(async move { mgr.record_state(&id, status, None).await })
}

// Fails if the save runs under the map lock again.
#[tokio::test]
async fn given_a_save_in_flight_when_the_map_is_read_then_it_answers() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let writer = Arc::clone(&mgr);
    let rename = tokio::spawn(async move { writer.rename(NAME, RENAMED).await });
    held.held().await;

    // Act
    let listed = tokio::time::timeout(LONG, mgr.list()).await;
    held.open();

    // Assert
    assert!(listed.is_ok(), "list waited for a record write");
    assert_eq!(rename.await.unwrap().unwrap().name, RENAMED);
}

// Fails if `finish_cleanup` stops waiting for the persist lock: the backend
// would delete the directory and the gated save would then recreate it.
#[tokio::test]
async fn given_a_save_mid_write_when_the_sandbox_is_removed_then_the_write_fails_and_no_record_stays()
 {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let write = spawn_record(&mgr, &created.id, ProtoStatus::Exited);
    held.held().await;
    let (remover, id) = (Arc::clone(&mgr), created.id.clone());
    let remove = tokio::spawn(async move { remover.remove(&id).await });
    until_removed_from_map(&mgr, &created.id).await;
    tokio::time::sleep(SETTLE).await;

    // Act
    held.open();
    remove.await.unwrap().unwrap();
    let wrote = write.await.unwrap();

    // Assert
    assert!(
        matches!(wrote, Err(ApiError::SandboxNotFound(_))),
        "{wrote:?}"
    );
    assert!(!record_dir(tmp.path(), &created.id).exists());
}

// Fails if the write and the adoption run in the caller's own future.
#[tokio::test]
async fn given_a_caller_dropped_mid_write_when_the_write_ends_then_memory_equals_disk() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let write = spawn_record(&mgr, &created.id, ProtoStatus::Exited);
    held.held().await;

    // Act
    write.abort();
    held.open();
    drop(mgr.teardown().lock_persist(&created.id).await);

    // Assert
    assert_eq!(memory_status(&mgr, &created.id).await, ProtoStatus::Exited);
    assert_eq!(disk_status(tmp.path()), ProtoStatus::Exited);
}

// Fails if a dropped caller frees the lock before its save ends.
#[tokio::test]
async fn given_a_caller_dropped_mid_write_when_the_sandbox_is_removed_then_no_record_is_recreated()
{
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let write = spawn_record(&mgr, &created.id, ProtoStatus::Exited);
    held.held().await;
    write.abort();
    let (remover, id) = (Arc::clone(&mgr), created.id.clone());
    let remove = tokio::spawn(async move { remover.remove(&id).await });
    until_removed_from_map(&mgr, &created.id).await;
    tokio::time::sleep(SETTLE).await;

    // Act
    held.open();
    remove.await.unwrap().unwrap();
    // The orphaned save has no handle to wait on.
    tokio::time::sleep(SETTLE).await;

    // Assert
    assert!(!record_dir(tmp.path(), &created.id).exists());
}

// Fails if the guard drops after the `running` commit.
#[tokio::test]
async fn given_a_start_between_its_commit_and_registration_when_stopped_then_the_stop_waits() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let comms = mgr
        .entries
        .read()
        .await
        .get(&created.id)
        .unwrap()
        .meta
        .options
        .comms
        .clone();
    let held = gate::arm("after-running-commit", &created.id);
    let (starter, id) = (Arc::clone(&mgr), created.id.clone());
    let start = tokio::spawn(async move { starter.commit_running_and_register(&id, comms).await });
    held.held().await;
    let (stopper, id) = (Arc::clone(&mgr), created.id.clone());
    let stop = tokio::spawn(async move { stopper.begin_stop(&id).await });

    // Act
    assert_blocked(&stop, "stop").await;
    held.open();

    // Assert
    start.await.unwrap().unwrap();
    assert!(stop.await.unwrap().unwrap().is_some());
}

// Fails if two writers can plan from the same state.
#[tokio::test]
async fn given_two_stops_when_one_is_mid_write_then_exactly_one_wins() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let spawn_stop = || {
        let (mgr, id) = (Arc::clone(&mgr), created.id.clone());
        tokio::spawn(async move { mgr.begin_stop(&id).await })
    };
    let first = spawn_stop();
    held.held().await;
    let second = spawn_stop();

    // Act
    assert_blocked(&second, "second stop").await;
    held.open();
    let results = [first.await.unwrap(), second.await.unwrap()];

    // Assert
    let won = results.iter().filter(|r| matches!(r, Ok(Some(_)))).count();
    assert_eq!(won, 1, "{results:?}");
    let raced = results
        .iter()
        .any(|r| matches!(r, Err(ApiError::Internal(_))));
    assert!(!raced, "two saves raced: {results:?}");
    assert_eq!(
        memory_status(&mgr, &created.id).await,
        ProtoStatus::Stopping
    );
}

// Fails if two writers can plan from the same state.
#[tokio::test]
async fn given_two_starts_when_one_is_mid_write_then_exactly_one_wins() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    mgr.record_state(&created.id, ProtoStatus::Stopped, None)
        .await
        .unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let spawn_start = || {
        let (mgr, id) = (Arc::clone(&mgr), created.id.clone());
        tokio::spawn(async move { mgr.begin_start(&id).await })
    };
    let first = spawn_start();
    held.held().await;
    let second = spawn_start();

    // Act
    assert_blocked(&second, "second start").await;
    held.open();
    let results = [first.await.unwrap(), second.await.unwrap()];

    // Assert
    let won = results.iter().filter(|r| matches!(r, Ok(Some(_)))).count();
    assert_eq!(won, 1, "{results:?}");
    let raced = results
        .iter()
        .any(|r| matches!(r, Err(ApiError::Internal(_))));
    assert!(!raced, "two saves raced: {results:?}");
}

// Fails if a restart claim plans from a record a start is replacing.
#[tokio::test]
async fn given_a_start_mid_write_when_a_restart_is_claimed_then_the_claim_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(
            CreateSandboxRequest {
                name: NAME.into(),
                ..with_restart("always", 0)
            },
            "",
        )
        .await
        .unwrap();
    mgr.record_state(&created.id, ProtoStatus::Exited, Some(1))
        .await
        .unwrap();
    let finished_at = mgr
        .entries
        .read()
        .await
        .get(&created.id)
        .unwrap()
        .meta
        .finished_at;
    let request = RestartRequest {
        id: created.id.clone(),
        exit_code: 1,
        ran_for: std::time::Duration::ZERO,
        finished_at,
    };
    let held = gate::arm(gate::SAVE, &created.id);
    let (starter, id) = (Arc::clone(&mgr), created.id.clone());
    let start = tokio::spawn(async move { starter.begin_start(&id).await });
    held.held().await;
    let claimer = Arc::clone(&mgr);
    let claim = tokio::spawn(async move { claimer.claim_restart(&request).await });

    // Act
    assert_blocked(&claim, "claim").await;
    held.open();

    // Assert
    start.await.unwrap().unwrap();
    assert!(!claim.await.unwrap());
    let entries = mgr.entries.read().await;
    let meta = &entries.get(&created.id).unwrap().meta;
    assert_eq!(
        (meta.status.clone(), meta.restart_count),
        (ProtoStatus::Creating, 0)
    );
    let on_disk = crate::sandbox::store::MetaStore::new(tmp.path())
        .load_all()
        .remove(0);
    assert_eq!(on_disk.restart_count, 0);
}

// Fails if the exit watcher plans from a state a stop is replacing.
#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_a_stop_mid_write_when_the_main_process_ends_then_only_one_die_is_reported() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, _) = host_manager_in(MAIN_OUTPUT_CAP, false, Some(tmp.path().to_path_buf()));
    let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 6"], false).await;
    let mut reader = mgr
        .events()
        .subscribe(Some(std::time::SystemTime::UNIX_EPOCH));
    let held = gate::arm(gate::SAVE, &id);
    let (stopper, stop_id) = (Arc::clone(&mgr), id.clone());
    let stop = tokio::spawn(async move { stopper.stop(&stop_id).await });
    held.held().await;

    // Act
    send_line(&mgr, &id, b"go\n").await;
    tokio::time::sleep(SETTLE).await;
    held.open();
    stop.await.unwrap().unwrap();

    // Assert
    let actions: Vec<EventAction> = next_actions(&mut reader, 4)
        .await
        .iter()
        .map(|(action, _)| *action)
        .collect();
    assert_eq!(
        actions,
        [
            EventAction::Create,
            EventAction::Start,
            EventAction::Die,
            EventAction::Stop
        ]
    );
    let extra = tokio::time::timeout(SETTLE, reader.next()).await;
    assert!(extra.is_err(), "a second die or event followed the stop");
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
    assert_eq!(memory_status(&mgr, &created.id).await, ProtoStatus::Running);
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
        .map(|status| spawn_record(&mgr, &created.id, status))
        .collect();
    for writer in writers {
        writer.await.unwrap().unwrap();
    }

    // Assert
    assert_eq!(
        disk_status(tmp.path()),
        memory_status(&mgr, &created.id).await
    );
}

// Fails if the TTL removal decides without the persist lock.
#[tokio::test]
async fn given_a_start_mid_write_when_the_exited_ttl_fires_then_the_sandbox_is_kept() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    mgr.record_state(&created.id, ProtoStatus::Exited, Some(0))
        .await
        .unwrap();
    let held = gate::arm(gate::SAVE, &created.id);
    let (starter, id) = (Arc::clone(&mgr), created.id.clone());
    let start = tokio::spawn(async move { starter.begin_start(&id).await });
    held.held().await;
    let (teardown, id) = (mgr.teardown(), created.id.clone());
    let ttl = tokio::spawn(async move { remove_if_still_ended(&teardown, &id).await });

    // Act
    assert_blocked(&ttl, "ttl removal").await;
    held.open();
    start.await.unwrap().unwrap();
    ttl.await.unwrap();

    // Assert
    assert_eq!(
        memory_status(&mgr, &created.id).await,
        ProtoStatus::Creating
    );
}

// Fails if the auto-removal decides without the persist lock.
#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_a_start_mid_write_when_the_auto_removal_runs_then_the_sandbox_is_kept() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, _) = host_manager_in(MAIN_OUTPUT_CAP, false, Some(tmp.path().to_path_buf()));
    let id = create_with_command(&mgr, &["sh", "-c", "exit 0"], false).await;
    wait_for_status(&mgr, &id, SandboxStatus::Exited).await;
    let ended_pid = mgr.main_pid(&id).await.expect("main pid");
    let held = gate::arm(gate::SAVE, &id);
    let (starter, start_id) = (Arc::clone(&mgr), id.clone());
    let start = tokio::spawn(async move { starter.begin_start(&start_id).await });
    held.held().await;
    let (teardown, remove_id) = (mgr.teardown(), id.clone());
    let removal =
        tokio::spawn(async move { auto_remove_ended(&teardown, &remove_id, &ended_pid).await });

    // Act
    assert_blocked(&removal, "auto-removal").await;
    held.open();
    start.await.unwrap().unwrap();
    removal.await.unwrap();

    // Assert
    assert_eq!(memory_status(&mgr, &id).await, ProtoStatus::Creating);
}

// Fails if the policy is registered in the caller's own future.
#[tokio::test]
async fn given_a_caller_dropped_after_the_running_commit_when_the_task_ends_then_the_policy_is_registered()
 {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named_req(), "").await.unwrap();
    let comms = mgr
        .entries
        .read()
        .await
        .get(&created.id)
        .unwrap()
        .meta
        .options
        .comms
        .clone();
    mgr.broker.deregister_sandbox(&created.id).await;
    let held = gate::arm("after-running-commit", &created.id);
    let (starter, id) = (Arc::clone(&mgr), created.id.clone());
    let start = tokio::spawn(async move { starter.commit_running_and_register(&id, comms).await });
    held.held().await;

    // Act
    start.abort();
    held.open();
    drop(mgr.teardown().lock_persist(&created.id).await);

    // Assert
    let published = mgr
        .broker
        .publish(&created.id, "topic", bytes::Bytes::new())
        .await;
    assert!(
        !matches!(published, Err(ApiError::SandboxNotFound(_))),
        "{published:?}"
    );
}

// Fails if the stop runs in the caller's own future.
#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_a_caller_dropped_while_stopping_when_the_stop_ends_then_the_sandbox_is_stopped() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, _) = host_manager_in(MAIN_OUTPUT_CAP, false, Some(tmp.path().to_path_buf()));
    let id = create_with_command(&mgr, &["sleep", "30"], false).await;
    let held = gate::arm(gate::SAVE, &id);
    let (stopper, stop_id) = (Arc::clone(&mgr), id.clone());
    let stop = tokio::spawn(async move { stopper.stop(&stop_id).await });
    held.held().await;

    // Act
    stop.abort();
    held.open();

    // Assert
    wait_for_status(&mgr, &id, SandboxStatus::Stopped).await;
}
