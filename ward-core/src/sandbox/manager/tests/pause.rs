// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Pausing and unpausing a sandbox through the manager.

use pretty_assertions::assert_eq;

use super::*;

const NAME: &str = "web1";
const MAX_MANAGER_SANDBOXES: usize = 4;
const EVENT_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

async fn running(mgr: &SandboxManager) -> String {
    mgr.create(named(NAME), "").await.unwrap().id
}

fn exec_request(id: &str) -> crate::pb::ExecRequest {
    crate::pb::ExecRequest {
        sandbox_id: id.to_string(),
        command: vec!["true".into()],
        ..Default::default()
    }
}

async fn actions(reader: &mut crate::sandbox::events::EventReader) -> Vec<EventAction> {
    let mut seen = Vec::new();
    while let Ok(Some(next)) = tokio::time::timeout(EVENT_WAIT, reader.next()).await {
        seen.push(next.event.action);
    }
    seen
}

#[tokio::test]
async fn given_running_sandbox_when_paused_then_info_reports_paused_and_running() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;

    // Act
    let paused = mgr.pause(NAME).await.unwrap();

    // Assert
    assert!(paused.paused);
    assert_eq!(paused.status, crate::pb::SandboxStatus::Running as i32);
    assert!(mgr.get(&id).await.unwrap().paused);
}

#[tokio::test]
async fn given_paused_sandbox_when_unpaused_then_info_is_no_longer_paused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();

    // Act
    let thawed = mgr.unpause(&id).await.unwrap();

    // Assert
    assert!(!thawed.paused);
    assert!(!mgr.get(&id).await.unwrap().paused);
}

#[tokio::test]
async fn given_paused_sandbox_when_paused_again_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();

    // Act
    let err = mgr.pause(&id).await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("is already paused"), "{err}");
}

#[tokio::test]
async fn given_running_sandbox_when_unpaused_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;

    // Act
    let err = mgr.unpause(&id).await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("is not paused"), "{err}");
}

#[tokio::test]
async fn given_stopped_sandbox_when_paused_then_it_is_not_running() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.stop_within(&id, Some(0)).await.unwrap();

    // Act
    let err = mgr.pause(&id).await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("is not running"), "{err}");
}

#[tokio::test]
async fn given_paused_sandbox_when_exec_then_it_is_refused_as_paused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();

    // Act
    let err = mgr.exec(exec_request(&id)).await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("is paused"), "{err}");
}

#[tokio::test]
async fn given_unpaused_sandbox_when_exec_then_it_runs() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();
    mgr.unpause(&id).await.unwrap();

    // Act
    let started = mgr.exec(exec_request(&id)).await;

    // Assert
    assert!(started.is_ok());
}

#[tokio::test]
async fn given_paused_sandbox_when_stopped_then_it_stops_and_is_not_paused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();

    // Act
    let stopped = mgr.stop_within(&id, Some(0)).await.unwrap();

    // Assert
    assert_eq!(stopped.status, crate::pb::SandboxStatus::Stopped as i32);
    assert!(!stopped.paused);
}

#[tokio::test]
async fn given_paused_sandbox_when_removed_then_it_is_gone() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();

    // Act
    mgr.remove(&id).await.unwrap();

    // Assert
    assert!(matches!(
        mgr.resolve(&id).await,
        Err(ApiError::SandboxNotFound(_))
    ));
}

#[tokio::test]
async fn given_paused_sandbox_when_started_again_then_it_is_not_paused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    mgr.pause(&id).await.unwrap();
    mgr.stop_within(&id, Some(0)).await.unwrap();

    // Act
    let started = mgr.start(&id).await.unwrap();

    // Assert
    assert!(!started.paused);
}

#[tokio::test]
async fn given_pause_and_unpause_when_events_read_then_both_are_announced_in_order() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = running(&mgr).await;
    let mut reader = mgr.events().subscribe(Some(std::time::UNIX_EPOCH));

    // Act
    mgr.pause(&id).await.unwrap();
    mgr.unpause(&id).await.unwrap();

    // Assert
    let seen = actions(&mut reader).await;
    assert_eq!(
        seen[seen.len() - 2..],
        [EventAction::Pause, EventAction::Unpause]
    );
}
