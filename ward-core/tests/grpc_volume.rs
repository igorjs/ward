// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the volume CRUD path over the gRPC wire.
//!
//! Unit tests in volume::manager already verify the in-process state
//! machine. These tests verify the gRPC layer wires that state machine
//! correctly: status codes, request decoding, and the validation
//! boundary all participate.
//!
//! Style: BDD names (given/when/then) with AAA bodies. Every test starts
//! a fresh in-process server via the shared `common::test_server` harness,
//! so they are hermetic and run in parallel.

mod common;

use tonic::Code;

use ward_core::pb::{CreateVolumeRequest, GetVolumeRequest, RemoveVolumeRequest};

// ---------------------------------------------------------------------------
// CREATE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_daemon_when_create_volume_then_returns_volume_info() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let resp = client
        .create_volume(CreateVolumeRequest {
            name: "demo".into(),
            size_mb: 256,
        })
        .await
        .expect("create should succeed");
    let v = resp.into_inner();

    // Assert: identity fields are populated; the daemon assigns a UUID.
    assert!(!v.id.is_empty(), "id must be assigned");
    assert_eq!(v.name, "demo");
    assert_eq!(v.size_mb, 256);
    assert!(v.created_at.is_some());
    assert!(!v.mount_path.is_empty());
}

#[tokio::test]
async fn given_existing_daemon_when_create_with_invalid_name_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_volume(CreateVolumeRequest {
            name: "name with spaces".into(),
            size_mb: 256,
        })
        .await
        .expect_err("invalid name must be rejected");

    // Assert: the daemon mapped the validator's InvalidRequest to a gRPC
    // InvalidArgument with a useful message text.
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("volume name"),
        "got: {}",
        err.message()
    );
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_volume_when_get_by_id_then_returns_same_info() {
    // Arrange
    let mut client = common::test_server().await;
    let created = client
        .create_volume(CreateVolumeRequest {
            name: "demo".into(),
            size_mb: 100,
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let fetched = client
        .get_volume(GetVolumeRequest {
            id: created.id.clone(),
        })
        .await
        .expect("get should succeed")
        .into_inner();

    // Assert: round-tripping through the wire preserves every field.
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, created.name);
    assert_eq!(fetched.size_mb, created.size_mb);
    assert_eq!(fetched.mount_path, created.mount_path);
}

#[tokio::test]
async fn given_unknown_id_when_get_volume_then_not_found_not_invalid() {
    // Arrange: well-formed UUID that the daemon has never seen.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .get_volume(GetVolumeRequest {
            id: "00000000-0000-0000-0000-000000000000".into(),
        })
        .await
        .expect_err("unknown id must error");

    // Assert: NotFound, NOT InvalidArgument. This distinction matters
    // because clients use it to decide between "user typo" and "no such
    // volume on this daemon".
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_malformed_id_when_get_volume_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .get_volume(GetVolumeRequest {
            id: "not-a-uuid-zzzz".into(),
        })
        .await
        .expect_err("malformed id must be rejected");

    // Assert: validator catches this before lookup.
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// LIST
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_daemon_when_list_volumes_then_returns_empty_list() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let resp = client.list_volumes(()).await.expect("list").into_inner();

    // Assert
    assert!(resp.volumes.is_empty());
}

#[tokio::test]
async fn given_two_created_volumes_when_list_then_both_appear() {
    // Arrange
    let mut client = common::test_server().await;
    client
        .create_volume(CreateVolumeRequest {
            name: "alpha".into(),
            size_mb: 100,
        })
        .await
        .unwrap();
    client
        .create_volume(CreateVolumeRequest {
            name: "beta".into(),
            size_mb: 200,
        })
        .await
        .unwrap();

    // Act
    let mut volumes = client
        .list_volumes(())
        .await
        .expect("list")
        .into_inner()
        .volumes;

    // Assert: both names present. HashMap order is unspecified, so we sort
    // before comparing for stable assertions.
    volumes.sort_by(|x, y| x.name.cmp(&y.name));
    let names: Vec<&str> = volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
}

// ---------------------------------------------------------------------------
// REMOVE
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_volume_when_remove_then_subsequent_get_returns_not_found() {
    // Arrange
    let mut client = common::test_server().await;
    let v = client
        .create_volume(CreateVolumeRequest {
            name: "demo".into(),
            size_mb: 100,
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    client
        .remove_volume(RemoveVolumeRequest { id: v.id.clone() })
        .await
        .expect("remove should succeed");

    // Assert: the volume is no longer retrievable.
    let err = client
        .get_volume(GetVolumeRequest { id: v.id })
        .await
        .expect_err("must be gone");
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_unknown_id_when_remove_volume_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .remove_volume(RemoveVolumeRequest {
            id: "00000000-0000-0000-0000-000000000000".into(),
        })
        .await
        .expect_err("removing unknown must fail");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

// ---------------------------------------------------------------------------
// CAPACITY CAP (the harness configures max_volumes = 4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_volumes_at_cap_when_create_one_more_then_invalid_argument_with_limit_message() {
    // Arrange: fill the manager up to the harness cap of 4.
    let mut client = common::test_server().await;
    for i in 0..4 {
        client
            .create_volume(CreateVolumeRequest {
                name: format!("v{i}"),
                size_mb: 10,
            })
            .await
            .expect("under cap");
    }

    // Act: the 5th request must be rejected.
    let err = client
        .create_volume(CreateVolumeRequest {
            name: "overflow".into(),
            size_mb: 10,
        })
        .await
        .expect_err("over cap");

    // Assert: InvalidArgument is the correct mapping (the cap is a client
    // problem — they asked for too many — not a server fault). Message
    // text mentions "limit" so users can grep their CI logs.
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("limit"),
        "expected 'limit' in: {}",
        err.message()
    );
}

#[tokio::test]
async fn given_volumes_at_cap_when_one_removed_then_create_again_succeeds() {
    // Arrange: regression for cap-counter bookkeeping. Fill cap, then
    // remove one, then create one more.
    let mut client = common::test_server().await;
    let mut ids = vec![];
    for i in 0..4 {
        let v = client
            .create_volume(CreateVolumeRequest {
                name: format!("v{i}"),
                size_mb: 10,
            })
            .await
            .unwrap()
            .into_inner();
        ids.push(v.id);
    }

    // Remove one.
    client
        .remove_volume(RemoveVolumeRequest { id: ids[0].clone() })
        .await
        .unwrap();

    // Act: should succeed now that a slot is free.
    let result = client
        .create_volume(CreateVolumeRequest {
            name: "replacement".into(),
            size_mb: 10,
        })
        .await;

    // Assert
    assert!(result.is_ok(), "removing a volume must free a cap slot");
}
