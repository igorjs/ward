// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the sandbox CRUD path over the gRPC wire.
//!
//! Unit tests in sandbox::manager already verify the in-process state
//! machine. These tests verify the gRPC layer wires that state machine
//! correctly: status codes (NotFound vs InvalidArgument), request
//! decoding, and the validation boundary all participate.
//!
//! Style: BDD names with AAA bodies. Every test starts a fresh in-process
//! server via `common::test_server`, so they are hermetic.

mod common;

use tonic::Code;

use ward_core::pb::{
    CommunicationMode as PbCommunicationMode, CommunicationPolicy as PbCommunicationPolicy,
    CreateSandboxRequest, GetSandboxRequest, RemoveSandboxRequest, ResourceLimits,
};

// ---------------------------------------------------------------------------
// CREATE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_daemon_when_create_sandbox_then_returns_sandbox_info() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let resp = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create should succeed");
    let s = resp.into_inner();

    // Assert: daemon-assigned UUID, image echoed back, status is Creating.
    assert!(!s.id.is_empty(), "id must be assigned");
    assert_eq!(s.image, "alpine:latest");
    assert_eq!(s.status, ward_core::pb::SandboxStatus::Creating as i32);
}

#[tokio::test]
async fn given_create_with_env_when_request_succeeds_then_env_does_not_leak_into_response() {
    // Arrange: env is set on the request but is never reflected back in
    // SandboxInfo (which only carries identity fields, not configuration).
    // This is a regression guard: SDKs should not start depending on env
    // values being readable from create() response.
    let mut client = common::test_server().await;
    let env: std::collections::HashMap<String, String> =
        [("FOO".to_string(), "bar".to_string())].into();

    // Act
    let resp = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            env,
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    // Assert: identity fields populated, no env smuggled in.
    assert!(!resp.id.is_empty());
    assert_eq!(resp.image, "alpine:latest");
}

#[tokio::test]
async fn given_empty_image_when_create_sandbox_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: String::new(),
            ..Default::default()
        })
        .await
        .expect_err("empty image must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_oversized_cpus_when_create_sandbox_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            resources: Some(ResourceLimits {
                cpus: 9999,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect_err("9999 cpus must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_group_mode_without_name_when_create_sandbox_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            comms: Some(PbCommunicationPolicy {
                mode: PbCommunicationMode::Group as i32,
                group: String::new(),
            }),
            ..Default::default()
        })
        .await
        .expect_err("group without name must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_sandbox_when_get_by_id_then_returns_same_info() {
    // Arrange
    let mut client = common::test_server().await;
    let created = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let fetched = client
        .get_sandbox(GetSandboxRequest {
            id: created.id.clone(),
        })
        .await
        .expect("get")
        .into_inner();

    // Assert
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.image, created.image);
}

#[tokio::test]
async fn given_unknown_id_when_get_sandbox_then_not_found_not_invalid() {
    // Arrange: well-formed but unknown ID. Validator passes it; lookup
    // fails. Verifies the manager's BackendError::NotFound → NotFound
    // mapping makes it through to the gRPC status code.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .get_sandbox(GetSandboxRequest {
            id: "00000000-0000-0000-0000-000000000000".into(),
        })
        .await
        .expect_err("unknown id");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_malformed_id_when_get_sandbox_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .get_sandbox(GetSandboxRequest {
            id: "not-a-uuid-zzzz".into(),
        })
        .await
        .expect_err("malformed id");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// LIST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_daemon_when_list_sandboxes_then_returns_empty_list() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let resp = client.list_sandboxes(()).await.expect("list").into_inner();

    // Assert
    assert!(resp.sandboxes.is_empty());
}

#[tokio::test]
async fn given_two_created_sandboxes_when_list_then_both_appear() {
    // Arrange
    let mut client = common::test_server().await;
    client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:a".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:b".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Act
    let mut sandboxes = client
        .list_sandboxes(())
        .await
        .expect("list")
        .into_inner()
        .sandboxes;

    // Assert: both images present, sorted to defeat HashMap order.
    sandboxes.sort_by(|x, y| x.image.cmp(&y.image));
    let images: Vec<&str> = sandboxes.iter().map(|s| s.image.as_str()).collect();
    assert_eq!(images, vec!["alpine:a", "alpine:b"]);
}

// ---------------------------------------------------------------------------
// REMOVE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_sandbox_when_remove_then_subsequent_get_returns_not_found() {
    // Arrange
    let mut client = common::test_server().await;
    let s = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    client
        .remove_sandbox(RemoveSandboxRequest { id: s.id.clone() })
        .await
        .expect("remove");

    // Assert
    let err = client
        .get_sandbox(GetSandboxRequest { id: s.id })
        .await
        .expect_err("must be gone");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_unknown_id_when_remove_sandbox_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .remove_sandbox(RemoveSandboxRequest {
            id: "00000000-0000-0000-0000-000000000000".into(),
        })
        .await
        .expect_err("unknown id");

    // Assert: NotFound, NOT InvalidArgument. This was a real bug caught
    // by the unit tests: previously the manager wrapped all backend
    // errors as ApiError::Backend, losing the NotFound distinction.
    assert_eq!(err.code(), Code::NotFound);
}

// ---------------------------------------------------------------------------
// CAPACITY CAP (the harness configures max_sandboxes = 4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_sandboxes_at_cap_when_create_one_more_then_invalid_argument() {
    // Arrange: fill the manager up to the harness cap of 4.
    let mut client = common::test_server().await;
    for i in 0..4 {
        client
            .create_sandbox(CreateSandboxRequest {
                image: format!("alpine:{i}"),
                ..Default::default()
            })
            .await
            .expect("under cap");
    }

    // Act
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:overflow".into(),
            ..Default::default()
        })
        .await
        .expect_err("over cap");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("limit"),
        "expected 'limit' in: {}",
        err.message()
    );
}

#[tokio::test]
async fn given_sandboxes_at_cap_when_one_removed_then_create_again_succeeds() {
    // Arrange: regression for cap-counter bookkeeping. Fill the cap, then
    // remove one, then create one more. If the bookkeeping leaks slots
    // (e.g. decrements on failure paths it shouldn't), this test catches
    // it. The volume manager has the symmetrical test; without this one,
    // a regression in the sandbox slot accounting would slip past
    // integration and only surface in production.
    let mut client = common::test_server().await;
    let mut ids = vec![];
    for i in 0..4 {
        let s = client
            .create_sandbox(CreateSandboxRequest {
                image: format!("alpine:{i}"),
                ..Default::default()
            })
            .await
            .expect("under cap")
            .into_inner();
        ids.push(s.id);
    }

    // Remove one to free a slot.
    client
        .remove_sandbox(RemoveSandboxRequest { id: ids[0].clone() })
        .await
        .expect("remove first sandbox");

    // Act
    let result = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:replacement".into(),
            ..Default::default()
        })
        .await;

    // Assert
    assert!(
        result.is_ok(),
        "removing a sandbox must free a cap slot, got: {result:?}"
    );
}
