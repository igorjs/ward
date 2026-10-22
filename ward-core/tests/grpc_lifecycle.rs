// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the Stop, Start and Restart RPCs: token gate,
//! name lookup, state changes, the sandbox cap and a daemon restart.
//!
//! Style: BDD names with AAA bodies, one fresh in-process server per test.

mod common;

use rstest::rstest;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};

use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    CreateSandboxRequest, GetSandboxRequest, RestartRequest, SandboxInfo, SandboxStatus,
    StartRequest, StopRequest,
};

const SANDBOX_NAME: &str = "web1";
const WRONG_TOKEN: &str = "00000000-0000-0000-0000-000000000000";
const MAX_SANDBOXES: usize = 4;
const DATA_FILE: &str = "data.txt";
const DATA_BYTES: &[u8] = b"kept across stop and start\n";

type Client = WardClient<Channel>;

#[derive(Debug, Clone, Copy)]
enum Rpc {
    Stop,
    Start,
    Restart,
}

async fn create_named(client: &mut Client, name: &str) -> SandboxInfo {
    client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            name: name.into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
}

async fn call(
    client: &mut Client,
    rpc: Rpc,
    reference: &str,
    token: Option<&str>,
) -> Result<SandboxInfo, Status> {
    let id = reference.to_string();
    macro_rules! send {
        ($method:ident, $message:expr) => {{
            let mut request = Request::new($message);
            if let Some(token) = token {
                request
                    .metadata_mut()
                    .insert("x-ward-sandbox-token", token.parse().expect("ascii token"));
            }
            client.$method(request).await.map(|r| r.into_inner())
        }};
    }
    match rpc {
        Rpc::Stop => send!(
            stop,
            StopRequest {
                id,
                ..Default::default()
            }
        ),
        Rpc::Start => send!(start, StartRequest { id }),
        Rpc::Restart => send!(
            restart,
            RestartRequest {
                id,
                ..Default::default()
            }
        ),
    }
}

async fn status_of(client: &mut Client, id: &str) -> SandboxStatus {
    let info = client
        .get_sandbox(GetSandboxRequest { id: id.into() })
        .await
        .expect("get")
        .into_inner();
    SandboxStatus::try_from(info.status).expect("known status")
}

#[rstest]
#[tokio::test]
async fn given_stop_start_or_restart_when_token_is_wrong_missing_or_empty_then_permission_denied(
    #[values(Rpc::Stop, Rpc::Start, Rpc::Restart)] rpc: Rpc,
    #[values(Some(WRONG_TOKEN), None, Some(""))] token: Option<&str>,
) {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_named(&mut client, "").await;

    // Act
    let status = call(&mut client, rpc, &created.id, token)
        .await
        .expect_err("a call without the right token must fail");

    // Assert
    assert_eq!(status.code(), Code::PermissionDenied, "{rpc:?}: {status:?}");
    assert_eq!(
        status_of(&mut client, &created.id).await,
        SandboxStatus::Running
    );
}

#[tokio::test]
async fn given_sandbox_named_web1_when_stop_called_with_the_name_then_that_sandbox_stops() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_named(&mut client, SANDBOX_NAME).await;

    // Act
    let info = call(&mut client, Rpc::Stop, SANDBOX_NAME, Some(&created.token))
        .await
        .expect("stop by name");

    // Assert
    assert_eq!(info.id, created.id);
    assert_eq!(info.status, SandboxStatus::Stopped as i32);
    assert_eq!(
        status_of(&mut client, &created.id).await,
        SandboxStatus::Stopped
    );
}

#[tokio::test]
async fn given_unique_id_prefix_when_stop_then_start_called_then_sandbox_runs_again() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_named(&mut client, "").await;
    let prefix = &created.id[..8];

    // Act
    call(&mut client, Rpc::Stop, prefix, Some(&created.token))
        .await
        .expect("stop by prefix");
    let started = call(&mut client, Rpc::Start, prefix, Some(&created.token))
        .await
        .expect("start by prefix");

    // Assert
    assert_eq!(started.status, SandboxStatus::Running as i32);
}

#[tokio::test]
async fn given_running_sandbox_when_restart_called_then_it_is_running() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_named(&mut client, "").await;

    // Act
    let info = call(&mut client, Rpc::Restart, &created.id, Some(&created.token))
        .await
        .expect("restart");

    // Assert
    assert_eq!(info.status, SandboxStatus::Running as i32);
}

#[tokio::test]
async fn given_unknown_sandbox_when_stop_called_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let status = call(&mut client, Rpc::Stop, SANDBOX_NAME, Some(WRONG_TOKEN))
        .await
        .expect_err("unknown sandbox");

    // Assert
    assert_eq!(status.code(), Code::NotFound);
}

#[tokio::test]
async fn given_max_sandboxes_with_one_stopped_when_another_is_created_then_it_is_refused() {
    // Arrange: fill every slot, then stop one of them.
    let mut client = common::test_server().await;
    let first = create_named(&mut client, "").await;
    for _ in 1..MAX_SANDBOXES {
        create_named(&mut client, "").await;
    }
    call(&mut client, Rpc::Stop, &first.id, Some(&first.token))
        .await
        .expect("stop");

    // Act
    let status = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect_err("a stopped sandbox keeps its slot");

    // Assert
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("limit"), "{status:?}");
}

#[tokio::test]
async fn given_stopped_sandbox_with_a_file_when_daemon_restarts_then_start_boots_and_file_is_there()
{
    // Arrange
    let data_dir = tempfile::tempdir().expect("tempdir");
    let mut before = common::persistent_server(data_dir.path()).await;
    let created = create_named(&mut before, SANDBOX_NAME).await;
    let file = data_dir
        .path()
        .join("sandboxes")
        .join(&created.id)
        .join("rootfs")
        .join(DATA_FILE);
    std::fs::write(&file, DATA_BYTES).expect("write file");
    call(&mut before, Rpc::Stop, &created.id, Some(&created.token))
        .await
        .expect("stop");
    let mut after = common::persistent_server(data_dir.path()).await;

    // Act
    let started = call(&mut after, Rpc::Start, SANDBOX_NAME, Some(&created.token))
        .await
        .expect("start with the stored token");

    // Assert
    assert_eq!(started.status, SandboxStatus::Running as i32);
    assert_eq!(std::fs::read(&file).expect("read file"), DATA_BYTES);
}

#[tokio::test]
async fn given_stopped_sandbox_after_daemon_restart_when_start_with_wrong_token_then_denied() {
    // Arrange
    let data_dir = tempfile::tempdir().expect("tempdir");
    let mut before = common::persistent_server(data_dir.path()).await;
    let created = create_named(&mut before, "").await;
    call(&mut before, Rpc::Stop, &created.id, Some(&created.token))
        .await
        .expect("stop");
    let mut after = common::persistent_server(data_dir.path()).await;

    // Act
    let status = call(&mut after, Rpc::Start, &created.id, Some(WRONG_TOKEN))
        .await
        .expect_err("wrong token");

    // Assert
    assert_eq!(status.code(), Code::PermissionDenied);
}
