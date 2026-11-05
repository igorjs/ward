// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The daemon, not the client, decides which volumes are anonymous and what
//! `volumes_from` shares. These tests drive crafted requests over gRPC.

mod common;

use tonic::Code;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    CreateSandboxRequest, CreateVolumeRequest, Mount, RemoveSandboxRequest, SandboxInfo,
};

type Client = WardClient<Channel>;

const IMAGE: &str = "alpine:latest";
const ANONYMOUS_NAME: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn volume_mount(name: &str, target: &str, anonymous: bool) -> Mount {
    Mount {
        volume: name.into(),
        target: target.into(),
        anonymous,
        ..Default::default()
    }
}

async fn create(
    client: &mut Client,
    mounts: Vec<Mount>,
    volumes_from: Vec<String>,
) -> Result<SandboxInfo, tonic::Status> {
    let created = client
        .create_sandbox(CreateSandboxRequest {
            image: IMAGE.into(),
            mounts,
            volumes_from,
            ..Default::default()
        })
        .await?
        .into_inner();
    // The create reply omits mounts, so read them back.
    let mut info = client
        .get_sandbox(ward_core::pb::GetSandboxRequest {
            id: created.id.clone(),
        })
        .await?
        .into_inner();
    info.token = created.token;
    Ok(info)
}

async fn volume_names(client: &mut Client) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_volumes(())
        .await
        .unwrap()
        .into_inner()
        .volumes
        .into_iter()
        .map(|v| v.name)
        .collect();
    names.sort();
    names
}

async fn remove(client: &mut Client, sandbox: &SandboxInfo, remove_volumes: bool) {
    let request = common::with_token(
        RemoveSandboxRequest {
            id: sandbox.id.clone(),
            remove_volumes,
        },
        &sandbox.token,
    );
    client.remove_sandbox(request).await.unwrap();
}

#[tokio::test]
async fn given_a_named_volume_marked_anonymous_when_created_then_the_daemon_refuses() {
    // Arrange
    let mut client = common::test_server().await;
    client
        .create_volume(CreateVolumeRequest {
            name: "precious".into(),
            size_mb: 10,
        })
        .await
        .unwrap();

    // Act
    let by_name = create(
        &mut client,
        vec![volume_mount("precious", "/d", true)],
        vec![],
    )
    .await;

    // Assert
    assert_eq!(by_name.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(volume_names(&mut client).await, ["precious"]);
}

#[tokio::test]
async fn given_an_existing_volume_with_a_hex_name_marked_anonymous_when_created_then_it_is_refused()
{
    // Arrange
    let mut client = common::test_server().await;
    client
        .create_volume(CreateVolumeRequest {
            name: ANONYMOUS_NAME.into(),
            size_mb: 10,
        })
        .await
        .unwrap();

    // Act
    let result = create(
        &mut client,
        vec![volume_mount(ANONYMOUS_NAME, "/d", true)],
        vec![],
    )
    .await;

    // Assert
    assert_eq!(result.unwrap_err().code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_a_new_hex_named_anonymous_volume_when_removed_with_volumes_then_it_goes() {
    // Arrange
    let mut client = common::test_server().await;
    let sandbox = create(
        &mut client,
        vec![volume_mount(ANONYMOUS_NAME, "/d", true)],
        vec![],
    )
    .await
    .unwrap();
    assert_eq!(volume_names(&mut client).await, [ANONYMOUS_NAME]);

    // Act
    remove(&mut client, &sandbox, true).await;

    // Assert
    assert!(volume_names(&mut client).await.is_empty());
}

#[tokio::test]
async fn given_a_named_volume_when_the_sandbox_is_removed_with_volumes_then_the_volume_stays() {
    // Arrange
    let mut client = common::test_server().await;
    let sandbox = create(&mut client, vec![volume_mount("keep", "/d", false)], vec![])
        .await
        .unwrap();

    // Act
    remove(&mut client, &sandbox, true).await;

    // Assert
    assert_eq!(volume_names(&mut client).await, ["keep"]);
}

#[tokio::test]
async fn given_volumes_from_when_created_then_the_daemon_copies_the_mounts_and_claims_them() {
    // Arrange
    let mut client = common::test_server().await;
    let owner = create(
        &mut client,
        vec![volume_mount(ANONYMOUS_NAME, "/anon", true)],
        vec![],
    )
    .await
    .unwrap();

    // Act
    let user = create(&mut client, vec![], vec![format!("{}:ro", owner.id)])
        .await
        .unwrap();
    remove(&mut client, &owner, true).await;
    let while_shared = volume_names(&mut client).await;
    remove(&mut client, &user, true).await;

    // Assert
    assert_eq!(user.mounts.len(), 1);
    assert!(user.mounts[0].readonly);
    assert_eq!(user.mounts[0].volume, ANONYMOUS_NAME);
    assert_eq!(while_shared, [ANONYMOUS_NAME]);
    assert!(volume_names(&mut client).await.is_empty());
}

#[tokio::test]
async fn given_an_explicit_mount_on_an_inherited_target_when_created_then_the_explicit_one_wins() {
    // Arrange
    let mut client = common::test_server().await;
    let owner = create(
        &mut client,
        vec![volume_mount("shared", "/data", false)],
        vec![],
    )
    .await
    .unwrap();

    // Act
    let user = create(
        &mut client,
        vec![volume_mount("mine", "/data", false)],
        vec![owner.id.clone()],
    )
    .await
    .unwrap();

    // Assert
    assert_eq!(user.mounts.len(), 1);
    assert_eq!(user.mounts[0].volume, "mine");
}

#[tokio::test]
async fn given_a_bad_or_unknown_volumes_from_when_created_then_it_fails_with_the_reason() {
    // Arrange
    let mut client = common::test_server().await;
    let owner = create(&mut client, vec![volume_mount("v", "/d", false)], vec![])
        .await
        .unwrap();

    // Act
    let bad_mode = create(&mut client, vec![], vec![format!("{}:wat", owner.id)]).await;
    let unknown = create(&mut client, vec![], vec!["no-such-sandbox".into()]).await;

    // Assert
    let bad_mode = bad_mode.unwrap_err();
    assert_eq!(bad_mode.code(), Code::InvalidArgument);
    assert!(bad_mode.message().contains("use ro or rw"), "{bad_mode}");
    assert_eq!(unknown.unwrap_err().code(), Code::NotFound);
}
