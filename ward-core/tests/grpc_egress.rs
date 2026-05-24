// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for GetEgressLog over the gRPC wire.
//!
//! The proxy's enforcement and logging are unit-tested in egress::proxy;
//! these verify the RPC wiring: a known sandbox returns its (initially
//! empty) log, and an unknown sandbox maps to NotFound.

mod common;

use tonic::Code;

use ward_core::pb::{CreateSandboxRequest, GetEgressLogRequest};

fn create_req() -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: "alpine:latest".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn given_existing_sandbox_when_get_egress_log_then_returns_empty_log() {
    // Arrange: create a sandbox so a proxy (and its log) exists.
    let mut client = common::test_server().await;
    let sandbox = client
        .create_sandbox(create_req())
        .await
        .expect("create")
        .into_inner();

    // Act
    let resp = client
        .get_egress_log(GetEgressLogRequest {
            sandbox_id: sandbox.id.clone(),
        })
        .await
        .expect("get_egress_log should succeed");

    // Assert: no traffic has been proxied yet, so the log is empty — but the
    // RPC is wired (no longer Unimplemented).
    assert!(resp.into_inner().entries.is_empty());
}

#[tokio::test]
async fn given_unknown_sandbox_when_get_egress_log_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let status = client
        .get_egress_log(GetEgressLogRequest {
            sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
        })
        .await
        .expect_err("unknown sandbox must error");

    // Assert
    assert_eq!(status.code(), Code::NotFound);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_get_egress_log_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act: a non-UUID id fails validation before lookup.
    let status = client
        .get_egress_log(GetEgressLogRequest {
            sandbox_id: "not a valid id".into(),
        })
        .await
        .expect_err("malformed id must error");

    // Assert
    assert_eq!(status.code(), Code::InvalidArgument);
}
