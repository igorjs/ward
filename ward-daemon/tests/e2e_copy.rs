// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the `CopyTo` and `CopyFrom` streams against a real
//! wardd subprocess. The byte-exact round trip needs a guest agent, so it
//! runs only against a real microVM (see `e2e_vm.rs` for how to enable it).

mod common;

use common::Daemon;
use tokio_stream::StreamExt;
use tonic::Code;
use tonic::metadata::MetadataValue;
use ward_core::pb::copy_to_message::Kind;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    CopyFromRequest, CopyToMessage, CopyToStart, CreateSandboxRequest, SandboxInfo,
};

const TOKEN_HEADER: &str = "x-ward-sandbox-token";
const GUEST_DIR: &str = "/tmp";
/// Longest the early-failure test waits before calling the call hung.
const GIVE_UP: std::time::Duration = std::time::Duration::from_secs(10);

type Client = WardClient<tonic::transport::Channel>;

async fn create(client: &mut Client) -> SandboxInfo {
    let request = CreateSandboxRequest {
        image: "alpine:latest".into(),
        command: vec!["sleep".into(), "600".into()],
        ..Default::default()
    };
    client
        .create_sandbox(request)
        .await
        .expect("create")
        .into_inner()
}

fn with_token<T>(mut request: tonic::Request<T>, token: Option<&str>) -> tonic::Request<T> {
    if let Some(token) = token {
        let value = MetadataValue::try_from(token).expect("ascii token");
        request.metadata_mut().insert(TOKEN_HEADER, value);
    }
    request
}

fn start_message(id: &str) -> CopyToMessage {
    CopyToMessage {
        kind: Some(Kind::Start(CopyToStart {
            sandbox_id: id.into(),
            path: GUEST_DIR.into(),
        })),
    }
}

async fn copy_to_status(client: &mut Client, id: &str, token: Option<&str>) -> Option<Code> {
    let request = with_token(
        tonic::Request::new(tokio_stream::iter(vec![start_message(id)])),
        token,
    );
    client.copy_to(request).await.err().map(|s| s.code())
}

async fn copy_from_status(client: &mut Client, id: &str, token: Option<&str>) -> Option<Code> {
    let request = with_token(
        tonic::Request::new(CopyFromRequest {
            sandbox_id: id.into(),
            path: GUEST_DIR.into(),
        }),
        token,
    );
    client.copy_from(request).await.err().map(|s| s.code())
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_wrong_missing_or_empty_token_when_copy_to_then_permission_denied() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client).await;

    for token in [Some("wrong"), None, Some("")] {
        // Act
        let code = copy_to_status(&mut client, &sandbox.id, token).await;

        // Assert
        assert_eq!(code, Some(Code::PermissionDenied), "token {token:?}");
    }
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_wrong_missing_or_empty_token_when_copy_from_then_permission_denied() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client).await;

    for token in [Some("wrong"), None, Some("")] {
        // Act
        let code = copy_from_status(&mut client, &sandbox.id, token).await;

        // Assert
        assert_eq!(code, Some(Code::PermissionDenied), "token {token:?}");
    }
}

#[tokio::test]
async fn given_an_unknown_sandbox_when_copy_from_then_not_found_in_docker_wording() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let request = tonic::Request::new(CopyFromRequest {
        sandbox_id: "ghost".into(),
        path: GUEST_DIR.into(),
    });

    // Act
    let status = client
        .copy_from(request)
        .await
        .expect_err("no such sandbox");

    // Assert
    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(status.message(), "No such container: ghost");
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_a_copy_that_fails_early_when_the_client_keeps_streaming_then_the_call_ends() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client).await;
    let open_stream =
        tokio_stream::iter(vec![start_message(&sandbox.id)]).chain(tokio_stream::pending());
    let request = with_token(tonic::Request::new(open_stream), Some(&sandbox.token));

    // Act
    let outcome = tokio::time::timeout(GIVE_UP, client.copy_to(request)).await;

    // Assert
    let status = outcome
        .expect("the call hung on a client that kept its stream open")
        .expect_err("a sandbox without a microVM cannot take a copy");
    assert_eq!(status.code(), Code::InvalidArgument);
}
