// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the snapshot RPCs (CreateSnapshot, RestoreSnapshot,
//! ListSnapshots).
//!
//! The snapshot backend is not yet implemented — the gRPC server returns
//! `Unimplemented` once inputs pass validation. These tests lock in the
//! input-validation contract: empty or malformed identity fields reject
//! with `InvalidArgument` BEFORE the unimplemented stub. When real
//! snapshots land, the negative tests stay valid; the positive tests
//! swap their assertion from `Unimplemented` to whatever the new
//! contract is.

mod common;

use tonic::Code;

use ward_core::pb::{CreateSnapshotRequest, ListSnapshotsRequest, RestoreSnapshotRequest};

// Well-formed UUID used wherever the test needs a "valid but non-existent" ID.
const VALID_UUID: &str = "00000000-0000-0000-0000-000000000000";

// ---------------------------------------------------------------------------
// CreateSnapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_valid_sandbox_id_when_create_snapshot_then_unimplemented_not_invalid() {
    // Arrange: validation passes, backend is unimplemented. This locks
    // in the layering: when the backend lands, only this assertion flips.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            label: "checkpoint-1".into(),
        })
        .await
        .expect_err("create_snapshot stub returns unimplemented");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}

#[tokio::test]
async fn given_empty_sandbox_id_when_create_snapshot_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: String::new(),
            label: "checkpoint-1".into(),
        })
        .await
        .expect_err("empty sandbox_id rejected");

    // Assert: must be InvalidArgument from the validator, not the
    // Unimplemented from the backend stub. Order matters — a wrong
    // ordering would mask validation regressions until the backend
    // ships.
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_create_snapshot_then_invalid_argument() {
    // Arrange: 'z' is not a hex character, so entity_id rejects it.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: "not-a-uuid-zzzz".into(),
            label: String::new(),
        })
        .await
        .expect_err("malformed sandbox_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// RestoreSnapshot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_valid_ids_when_restore_snapshot_then_unimplemented_not_invalid() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            snapshot_id: VALID_UUID.into(),
        })
        .await
        .expect_err("restore_snapshot stub returns unimplemented");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}

#[tokio::test]
async fn given_empty_sandbox_id_when_restore_snapshot_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: String::new(),
            snapshot_id: VALID_UUID.into(),
        })
        .await
        .expect_err("empty sandbox_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_empty_snapshot_id_when_restore_snapshot_then_invalid_argument() {
    // Arrange: sandbox_id is valid, snapshot_id is empty — validates that
    // BOTH identity fields run through entity_id, not just the first.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            snapshot_id: String::new(),
        })
        .await
        .expect_err("empty snapshot_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("snapshot"),
        "error message should name the offending field, got: {}",
        err.message(),
    );
}

#[tokio::test]
async fn given_malformed_snapshot_id_when_restore_snapshot_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            snapshot_id: "not-hex-zzz".into(),
        })
        .await
        .expect_err("malformed snapshot_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// ListSnapshots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_valid_sandbox_id_when_list_snapshots_then_unimplemented_not_invalid() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .list_snapshots(ListSnapshotsRequest {
            sandbox_id: VALID_UUID.into(),
        })
        .await
        .expect_err("list_snapshots stub returns unimplemented");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}

#[tokio::test]
async fn given_empty_sandbox_id_when_list_snapshots_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .list_snapshots(ListSnapshotsRequest {
            sandbox_id: String::new(),
        })
        .await
        .expect_err("empty sandbox_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_list_snapshots_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .list_snapshots(ListSnapshotsRequest {
            sandbox_id: "not-a-uuid-zzz".into(),
        })
        .await
        .expect_err("malformed sandbox_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}
