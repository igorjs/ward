// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the cross-sandbox communication RPCs.
//!
//! The broker is unimplemented today; these tests lock in two contracts
//! that hold regardless:
//!
//!   1. **Validation runs before the unimplemented stub.** Malformed
//!      requests must return Code::InvalidArgument, not Unimplemented —
//!      otherwise the message "this feature is missing" would mask
//!      "your input was bad" and SDKs would have no way to distinguish.
//!
//!   2. **Valid inputs reach the stub.** A well-formed publish or
//!      subscribe must return Code::Unimplemented, not InvalidArgument.
//!      If the validator over-rejected, the future broker would never
//!      get the chance to handle the request.
//!
//! When the broker lands, this file expands with positive cases
//! ("publish then receive on subscriber"); the negative-path contracts
//! defined here continue to hold.
//!
//! Style: BDD names, AAA bodies. Each test starts a fresh server.

mod common;

use tonic::Code;

use ward_core::pb::{PublishRequest, SubscribeRequest};

// ---------------------------------------------------------------------------
// publish — validation boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_sandbox_id_when_publish_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: String::new(),
            topic: "events".into(),
            payload: vec![],
        })
        .await
        .expect_err("empty sandbox_id must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_publish_then_invalid_argument() {
    // Arrange: non-hex characters fail entity_id validation.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "not-a-uuid-zzzz".into(),
            topic: "events".into(),
            payload: vec![],
        })
        .await
        .expect_err("malformed sandbox_id must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_empty_topic_when_publish_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: String::new(),
            payload: vec![],
        })
        .await
        .expect_err("empty topic must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_topic_with_leading_dot_when_publish_then_invalid_argument() {
    // Arrange: leading-dot topics ("..events") are ambiguous for routing
    // and the validator rejects them before they reach the broker.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: ".events".into(),
            payload: vec![],
        })
        .await
        .expect_err("leading-dot topic must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_oversized_payload_when_publish_then_invalid_argument() {
    // Arrange: 2 MiB exceeds the 1 MiB cap. The validator rejects it
    // before any broker allocation — this is also a DoS defence so the
    // daemon never copies the oversized blob into its own memory.
    let mut client = common::test_server().await;
    let payload = vec![0u8; 2 * 1024 * 1024];

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: "events".into(),
            payload,
        })
        .await
        .expect_err("oversized payload must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// publish — "valid inputs reach the unimplemented stub" contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_valid_publish_when_request_succeeds_then_unimplemented_not_invalid() {
    // Arrange: a well-formed request. If the validator wrongly rejected
    // this we would see InvalidArgument and never notice when the
    // broker lands. Unimplemented is the contract today.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: "agent.results.build".into(),
            payload: b"hello".to_vec(),
        })
        .await
        .expect_err("publish stub returns unimplemented");

    // Assert
    assert_eq!(
        err.code(),
        Code::Unimplemented,
        "valid inputs must reach the broker stub, not be rejected by the validator",
    );
}

#[tokio::test]
async fn given_empty_payload_when_publish_then_unimplemented_not_invalid() {
    // Arrange: empty payloads are explicitly valid (ping-style messages).
    // The validator must accept them so future "ping over the bus" use
    // cases work without payload-encoding gymnastics.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: "agent.heartbeat".into(),
            payload: vec![],
        })
        .await
        .expect_err("empty payload still reaches the stub");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}

// ---------------------------------------------------------------------------
// subscribe — validation boundary + stub contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_empty_sandbox_id_when_subscribe_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: String::new(),
            topic: "events".into(),
        })
        .await
        .expect_err("empty sandbox_id must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_empty_topic_when_subscribe_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: "deadbeef".into(),
            topic: String::new(),
        })
        .await
        .expect_err("empty topic must be rejected");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_valid_subscribe_when_request_succeeds_then_unimplemented_not_invalid() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: "deadbeef".into(),
            topic: "agent.events".into(),
        })
        .await
        .expect_err("subscribe stub returns unimplemented");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}
