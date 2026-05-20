// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the snapshot RPCs (CreateSnapshot,
//! RestoreSnapshot, ListSnapshots).
//!
//! The backend is currently a stub (real libkrun checkpoint integration
//! lands later). That stub tracks metadata in-memory and rejects bad
//! inputs the same way the real backend will, so the contracts here
//! survive the eventual implementation swap:
//!
//!   - Validation runs before the backend; malformed ids reject as
//!     InvalidArgument.
//!   - Unknown sandbox/snapshot ids surface as NotFound, never Internal.
//!   - Cross-sandbox restore returns NotFound (tenant isolation guard).
//!   - list_snapshots is lenient on unknown sandboxes (empty list).
//!
//! Style: BDD names, AAA bodies. Each test starts a fresh in-process
//! server via `common::test_server`, so they are hermetic.

mod common;

use tonic::Code;

use ward_core::pb::{
    CreateSandboxRequest, CreateSnapshotRequest, ListSnapshotsRequest, RestoreSnapshotRequest,
};

// Well-formed UUID for "valid but non-existent" cases.
const VALID_UUID: &str = "00000000-0000-0000-0000-000000000000";

// ---------------------------------------------------------------------------
// CreateSnapshot — validation boundary + happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_sandbox_id_when_create_snapshot_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: String::new(),
            label: "x".into(),
        })
        .await
        .expect_err("empty sandbox_id rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_create_snapshot_then_invalid_argument() {
    // Arrange
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

#[tokio::test]
async fn given_unknown_sandbox_when_create_snapshot_then_not_found() {
    // Arrange: well-formed UUID, no matching sandbox.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            label: "x".into(),
        })
        .await
        .expect_err("unknown sandbox");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_existing_sandbox_when_create_snapshot_then_returns_info_with_new_id() {
    // Arrange
    let mut client = common::test_server().await;
    let s = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let snap = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s.id.clone(),
            label: "checkpoint-1".into(),
        })
        .await
        .expect("create_snapshot")
        .into_inner();

    // Assert: fresh UUID, label round-trips, sandbox_id matches.
    assert_eq!(snap.snapshot_id.len(), 36);
    assert_eq!(snap.sandbox_id, s.id);
    assert_eq!(snap.label, "checkpoint-1");
}

// ---------------------------------------------------------------------------
// RestoreSnapshot — validation + lookup contracts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_malformed_snapshot_id_when_restore_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            snapshot_id: "not-hex-zzz".into(),
        })
        .await
        .expect_err("malformed snapshot_id");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_unknown_snapshot_when_restore_then_not_found() {
    // Arrange: well-formed but no snapshot exists.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: VALID_UUID.into(),
            snapshot_id: VALID_UUID.into(),
        })
        .await
        .expect_err("unknown snapshot");

    // Assert: NotFound, message mentions snapshot (not sandbox) so the
    // user knows which entity to investigate.
    assert_eq!(err.code(), Code::NotFound);
    assert!(
        err.message().contains("snapshot"),
        "expected 'snapshot' in: {}",
        err.message()
    );
}

#[tokio::test]
async fn given_snapshot_of_other_sandbox_when_restore_then_not_found() {
    // Arrange: tenant isolation — snapshot belongs to sb1, restoring it
    // as sb2 must look like the snapshot doesn't exist for sb2.
    let mut client = common::test_server().await;
    let s1 = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:1".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let s2 = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:2".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let snap = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s1.id,
            label: "x".into(),
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: s2.id,
            snapshot_id: snap.snapshot_id,
        })
        .await
        .expect_err("cross-sandbox restore");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_existing_snapshot_when_restore_with_correct_owner_then_ok() {
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
    let snap = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s.id.clone(),
            label: "before-change".into(),
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let resp = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: s.id,
            snapshot_id: snap.snapshot_id,
        })
        .await
        .expect("restore_snapshot");

    // Assert: Empty response on the wire becomes a unit Response.
    let _: () = resp.into_inner();
}

// ---------------------------------------------------------------------------
// ListSnapshots — validation + happy paths
// ---------------------------------------------------------------------------

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
        .expect_err("empty sandbox_id");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_unknown_sandbox_when_list_snapshots_then_empty_response() {
    // Arrange: list is lenient — unknown sandbox returns empty, not
    // NotFound. Callers commonly use list-as-exists-check.
    let mut client = common::test_server().await;

    // Act
    let resp = client
        .list_snapshots(ListSnapshotsRequest {
            sandbox_id: VALID_UUID.into(),
        })
        .await
        .expect("list_snapshots tolerates unknown sandbox")
        .into_inner();

    // Assert
    assert!(resp.snapshots.is_empty());
}

#[tokio::test]
async fn given_two_snapshots_when_list_then_returns_both_for_that_sandbox() {
    // Arrange: two snapshots of the same sandbox; list returns both
    // and only those (no leakage from a separate sandbox).
    let mut client = common::test_server().await;
    let s = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let other = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:other".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s.id.clone(),
            label: "first".into(),
        })
        .await
        .unwrap();
    client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s.id.clone(),
            label: "second".into(),
        })
        .await
        .unwrap();
    client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: other.id,
            label: "third".into(),
        })
        .await
        .unwrap();

    // Act
    let resp = client
        .list_snapshots(ListSnapshotsRequest {
            sandbox_id: s.id.clone(),
        })
        .await
        .unwrap()
        .into_inner();

    // Assert: exactly two, both scoped to this sandbox.
    assert_eq!(resp.snapshots.len(), 2);
    assert!(resp.snapshots.iter().all(|sn| sn.sandbox_id == s.id));
    let labels: Vec<&str> = resp.snapshots.iter().map(|sn| sn.label.as_str()).collect();
    assert!(labels.contains(&"first"));
    assert!(labels.contains(&"second"));
}

// ---------------------------------------------------------------------------
// Lifecycle: removing a sandbox reaps its snapshots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_sandbox_removed_when_restore_old_snapshot_then_not_found() {
    // Arrange: create + snapshot + remove the sandbox. The snapshot
    // should no longer be reachable.
    let mut client = common::test_server().await;
    let s = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let snap = client
        .create_snapshot(CreateSnapshotRequest {
            sandbox_id: s.id.clone(),
            label: "before-remove".into(),
        })
        .await
        .unwrap()
        .into_inner();
    client
        .remove_sandbox(ward_core::pb::RemoveSandboxRequest { id: s.id.clone() })
        .await
        .unwrap();

    // A fresh sandbox to provide a valid sandbox_id for the restore call.
    let s2 = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:replacement".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let err = client
        .restore_snapshot(RestoreSnapshotRequest {
            sandbox_id: s2.id,
            snapshot_id: snap.snapshot_id,
        })
        .await
        .expect_err("snapshot dangling after sandbox removed");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}
