// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Waiting with Docker's conditions. Real shell processes stand in for the
//! sandbox's main command.

#![cfg(not(feature = "krunvm"))]

use pretty_assertions::assert_eq;

use super::*;
use crate::pb::WaitCondition;

const EXIT_CODE: i32 = 6;
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
