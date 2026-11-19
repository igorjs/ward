// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Waiting with Docker's conditions. Real shell processes stand in for the
//! sandbox's main command.

#![cfg(not(feature = "krunvm"))]

use pretty_assertions::assert_eq;

use super::*;
use crate::pb::WaitCondition;

const EXIT_CODE: i32 = 6;
const LONG: std::time::Duration = std::time::Duration::from_secs(30);
const SHORT: std::time::Duration = std::time::Duration::from_millis(300);

fn spawn_wait_until(
    mgr: &Arc<SandboxManager>,
    id: &str,
    condition: WaitCondition,
) -> tokio::task::JoinHandle<Result<i32>> {
    let (mgr, id) = (Arc::clone(mgr), id.to_string());
    tokio::spawn(async move { mgr.wait_until(&id, condition).await })
}

#[tokio::test]
async fn given_a_running_sandbox_when_waited_for_not_running_then_it_returns_the_exit_code() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 6"], false).await;
    let waiter = spawn_wait_until(&mgr, &id, WaitCondition::NotRunning);

    // Act
    send_line(&mgr, &id, b"go\n").await;
    let got = waiter.await.expect("join").expect("wait");

    // Assert
    assert_eq!(got, EXIT_CODE);
}

#[tokio::test]
async fn given_an_exited_sandbox_when_waited_for_next_exit_then_it_waits_for_the_next_run() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "read x; exit $x"], false).await;
    send_line(&mgr, &id, b"1\n").await;
    assert_eq!(mgr.wait(&id).await.expect("first wait"), 1);
    let mut waiter = spawn_wait_until(&mgr, &id, WaitCondition::NextExit);
    let early = tokio::time::timeout(SHORT, &mut waiter).await;
    assert!(
        early.is_err(),
        "next-exit returned for the exit that was over"
    );

    // Act
    mgr.start(&id).await.expect("start");
    send_line(&mgr, &id, b"6\n").await;
    let got = waiter.await.expect("join").expect("wait");

    // Assert
    assert_eq!(got, EXIT_CODE);
}

/// The run starts and ends before the waiter first looks, which is what a
/// poll sees when the run is shorter than the poll interval.
#[tokio::test]
async fn given_a_run_that_started_and_ended_after_the_baseline_when_awaited_then_it_counts() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "exit 6"], false).await;
    mgr.start(&id).await.expect("first start");
    mgr.wait(&id).await.expect("first wait");
    let baseline = mgr.runs_started(&id).await.expect("baseline");
    mgr.start(&id).await.expect("second start");
    mgr.wait(&id).await.expect("second wait");

    // Act
    let seen = tokio::time::timeout(LONG, mgr.await_run_after(&id, baseline)).await;

    // Assert
    assert!(seen.expect("the run was missed").is_ok());
}

#[tokio::test]
async fn given_an_exited_sandbox_when_removed_during_a_next_exit_wait_then_it_is_not_found() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "exit 6"], false).await;
    mgr.start(&id).await.expect("start");
    mgr.wait(&id).await.expect("wait");
    let waiter = spawn_wait_until(&mgr, &id, WaitCondition::NextExit);

    // Act
    mgr.remove(&id).await.expect("remove");
    let got = tokio::time::timeout(LONG, waiter).await;

    // Assert
    let err = got.expect("next-exit hung").expect("join").unwrap_err();
    assert!(matches!(err, ApiError::SandboxNotFound(_)));
}

#[tokio::test]
async fn given_a_running_sandbox_when_waited_for_next_exit_then_it_returns_when_that_run_ends() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 6"], false).await;
    let waiter = spawn_wait_until(&mgr, &id, WaitCondition::NextExit);

    // Act
    send_line(&mgr, &id, b"go\n").await;
    let got = waiter.await.expect("join").expect("wait");

    // Assert
    assert_eq!(got, EXIT_CODE);
}

#[tokio::test]
async fn given_a_sandbox_when_waited_for_removed_then_it_returns_after_removal_with_the_exit_code()
{
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 6"], false).await;
    let mut waiter = spawn_wait_until(&mgr, &id, WaitCondition::Removed);
    send_line(&mgr, &id, b"go\n").await;
    mgr.wait(&id).await.expect("exited");
    let early = tokio::time::timeout(SHORT, &mut waiter).await;
    assert!(early.is_err(), "removed returned while the sandbox existed");

    // Act
    mgr.remove(&id).await.expect("remove");
    let got = waiter.await.expect("join").expect("wait");

    // Assert
    assert_eq!(got, EXIT_CODE);
}

#[tokio::test]
async fn given_an_auto_remove_sandbox_when_waited_for_removed_then_the_exit_code_survives() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let req = CreateSandboxRequest {
        image: "alpine".into(),
        command: vec!["sh".into(), "-c".into(), "read x; exit 6".into()],
        auto_remove: true,
        ..Default::default()
    };
    let id = mgr.create(req, "").await.expect("create").id;
    let waiter = spawn_wait_until(&mgr, &id, WaitCondition::Removed);

    // Act
    send_line(&mgr, &id, b"go\n").await;
    let got = waiter.await.expect("join").expect("wait");

    // Assert
    assert_eq!(got, EXIT_CODE);
}

#[tokio::test]
async fn given_an_unknown_sandbox_when_waited_for_removed_then_it_is_not_found() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);

    // Act
    let err = mgr
        .wait_until("ghost", WaitCondition::Removed)
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::SandboxNotFound(_)));
}

// Fails if `wait` falls back to polling the record.
#[tokio::test]
async fn given_a_record_changed_without_a_tick_when_waited_then_wait_sleeps_until_the_tick() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &[], false).await;
    let waiter = tokio::spawn({
        let (mgr, id) = (Arc::clone(&mgr), id.clone());
        async move { mgr.wait(&id).await }
    });
    tokio::time::sleep(SHORT).await;
    mgr.entries.write().await.get_mut(&id).unwrap().meta.status = ProtoStatus::Stopped;

    // Act
    tokio::time::sleep(SHORT).await;
    let slept = !waiter.is_finished();
    mgr.run_changes.send_replace(());

    // Assert
    assert!(slept, "wait looked at the record without a tick");
    assert_eq!(waiter.await.unwrap().unwrap(), 0);
}

// Fails if the main process entry lands without a tick: a waiter that woke
// on the `running` record would never learn about the run's main process.
#[tokio::test]
async fn given_a_start_held_before_its_main_process_when_it_registers_then_the_notifier_ticks() {
    // Arrange
    let mgr = host_manager(MAIN_OUTPUT_CAP);
    let id = create_with_command(&mgr, &["sleep", "30"], false).await;
    mgr.stop(&id).await.unwrap();
    let held = crate::sandbox::store::gate::arm("after-running-commit", &id);
    let (starter, start_id) = (Arc::clone(&mgr), id.clone());
    let start = tokio::spawn(async move { starter.start(&start_id).await });
    held.held().await;
    let ticks = mgr.run_changes.subscribe();

    // Act
    held.open();
    start.await.unwrap().unwrap();

    // Assert
    assert!(ticks.has_changed().unwrap());
}
