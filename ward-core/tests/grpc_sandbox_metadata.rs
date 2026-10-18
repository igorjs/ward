// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the name, command, workdir and exit code fields
//! on `SandboxInfo`, and for the token gate on the full command line.
//!
//! Style: BDD names with AAA bodies, one fresh in-process server per test.

mod common;

use tonic::{Code, Request};

use ward_core::pb::{CreateSandboxRequest, GetSandboxRequest, SandboxInfo};

const SECRET_ARG: &str = "--password=hunter2";
const COMMAND_NAME: &str = "serve";
const SANDBOX_NAME: &str = "web1";
const WORKDIR: &str = "/srv/app";

fn full_command() -> Vec<String> {
    vec![COMMAND_NAME.to_string(), SECRET_ARG.to_string()]
}

fn request_with_metadata() -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: "alpine:latest".into(),
        name: SANDBOX_NAME.into(),
        command: full_command(),
        workdir: WORKDIR.into(),
        ..Default::default()
    }
}

async fn create_with_metadata(
    client: &mut ward_core::pb::ward_client::WardClient<tonic::transport::Channel>,
) -> SandboxInfo {
    client
        .create_sandbox(request_with_metadata())
        .await
        .expect("create")
        .into_inner()
}

#[tokio::test]
async fn given_create_with_metadata_when_info_read_with_token_then_fields_match() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_with_metadata(&mut client).await;

    // Act
    let info = client
        .get_sandbox(common::with_token(
            GetSandboxRequest {
                id: created.id.clone(),
            },
            &created.token,
        ))
        .await
        .expect("get with valid token")
        .into_inner();

    // Assert
    assert_eq!(info.name, SANDBOX_NAME);
    assert_eq!(info.command, full_command());
    assert_eq!(info.command_name, COMMAND_NAME);
    assert_eq!(info.workdir, WORKDIR);
    assert_eq!(info.exit_code, None);
}

#[tokio::test]
async fn given_secret_argument_when_get_without_token_then_only_argv0_is_returned() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_with_metadata(&mut client).await;

    // Act
    let info = client
        .get_sandbox(GetSandboxRequest { id: created.id })
        .await
        .expect("tokenless get")
        .into_inner();

    // Assert
    assert_eq!(info.command_name, COMMAND_NAME);
    assert!(info.command.is_empty());
    assert!(!format!("{info:?}").contains(SECRET_ARG));
}

#[tokio::test]
async fn given_secret_argument_when_list_then_only_argv0_is_returned() {
    // Arrange
    let mut client = common::test_server().await;
    let created = create_with_metadata(&mut client).await;

    // Act
    let listed = client
        .list_sandboxes(())
        .await
        .expect("list")
        .into_inner()
        .sandboxes;

    // Assert
    let info = listed
        .iter()
        .find(|s| s.id == created.id)
        .expect("sandbox is listed");
    assert_eq!(info.command_name, COMMAND_NAME);
    assert!(info.command.is_empty());
    assert!(!format!("{listed:?}").contains(SECRET_ARG));
}

#[tokio::test]
async fn given_wrong_or_empty_token_when_get_then_permission_denied() {
    for token in ["not-the-real-token", ""] {
        // Arrange
        let mut client = common::test_server().await;
        let created = create_with_metadata(&mut client).await;

        // Act
        let status = client
            .get_sandbox(common::with_token(
                GetSandboxRequest { id: created.id },
                token,
            ))
            .await
            .expect_err("a presented but invalid token must be rejected");

        // Assert
        assert_eq!(status.code(), Code::PermissionDenied, "token {token:?}");
    }
}

#[tokio::test]
async fn given_old_client_without_new_fields_when_create_and_get_then_defaults_are_returned() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let created = client
        .create_sandbox(Request::new(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        }))
        .await
        .expect("create without the new fields")
        .into_inner();
    let info = client
        .get_sandbox(GetSandboxRequest { id: created.id })
        .await
        .expect("get")
        .into_inner();

    // Assert
    assert!(info.name.is_empty());
    assert!(info.workdir.is_empty());
    assert!(info.command_name.is_empty());
    assert!(info.command.is_empty());
    assert_eq!(info.exit_code, None);
}
