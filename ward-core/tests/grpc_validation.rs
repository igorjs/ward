// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests proving validators are wired into the gRPC request path.
//!
//! Unit tests confirm validators reject bad input. These tests confirm the
//! validators actually run when a real client sends a malformed request
//! over the wire. They catch the "I forgot the validation call" bug, which
//! unit tests alone cannot.
//!
//! Every assertion checks the gRPC status *code*, not just the error text.
//! `Code::InvalidArgument` is the contract for "your input was malformed";
//! anything else (e.g. `Internal`, `Unimplemented`) signals broken wiring.

mod common;

use tonic::Code;

use ward_core::pb::{
    CommunicationMode, CommunicationPolicy, CreateSandboxRequest, GetSandboxRequest,
    PublishRequest, RemoveSandboxRequest, RemoveVolumeRequest, ResourceLimits, SubscribeRequest,
};

// ---------------------------------------------------------------------------
// create_sandbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_sandbox_rejects_empty_image() {
    let mut client = common::test_server().await;

    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: String::new(),
            ..Default::default()
        })
        .await
        .expect_err("empty image must be rejected");

    assert_eq!(
        err.code(),
        Code::InvalidArgument,
        "got status: {} ({})",
        err.code(),
        err.message()
    );
}

#[tokio::test]
async fn create_sandbox_rejects_path_traversal_in_image() {
    let mut client = common::test_server().await;

    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "../../etc/passwd".into(),
            ..Default::default()
        })
        .await
        .expect_err("path traversal must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn create_sandbox_rejects_shell_metacharacters_in_image() {
    let mut client = common::test_server().await;

    // Shell injection via backticks would matter if the image string ever
    // reached a shell — the validator blocks it before that becomes possible.
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine`whoami`".into(),
            ..Default::default()
        })
        .await
        .expect_err("backticks must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn create_sandbox_rejects_oversized_cpus() {
    let mut client = common::test_server().await;

    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            resources: Some(ResourceLimits {
                cpus: 9999,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect_err("9999 CPUs must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn create_sandbox_rejects_group_mode_without_group_name() {
    let mut client = common::test_server().await;

    // CommunicationMode::Group requires a non-empty group string. The
    // validator catches this before the sandbox is created.
    let err = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            comms: Some(CommunicationPolicy {
                mode: CommunicationMode::Group as i32,
                group: String::new(),
            }),
            ..Default::default()
        })
        .await
        .expect_err("group mode without group name must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// get_sandbox / remove_sandbox / remove_volume — entity_id validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_sandbox_rejects_malformed_id() {
    let mut client = common::test_server().await;

    // Non-hex characters in an ID can never come from generateID(), so
    // there's no point even attempting the lookup. The validator returns
    // InvalidArgument; a missing-but-well-formed ID would return NotFound.
    let err = client
        .get_sandbox(GetSandboxRequest {
            id: "not-a-valid-uuid-zzzz".into(),
        })
        .await
        .expect_err("malformed ID must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn get_sandbox_unknown_id_returns_not_found_not_invalid() {
    let mut client = common::test_server().await;

    // Well-formed but unknown — distinct from malformed. Tests that the
    // validator passes a syntactically-valid ID through, and the lookup
    // returns NotFound rather than InvalidArgument.
    let err = client
        .get_sandbox(GetSandboxRequest {
            id: "deadbeef".into(),
        })
        .await
        .expect_err("unknown ID must error");

    assert_eq!(
        err.code(),
        Code::NotFound,
        "well-formed but unknown IDs must surface as NotFound, not InvalidArgument"
    );
}

#[tokio::test]
async fn remove_sandbox_rejects_empty_id() {
    let mut client = common::test_server().await;

    let err = client
        .remove_sandbox(RemoveSandboxRequest { id: String::new() })
        .await
        .expect_err("empty ID must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn remove_volume_rejects_empty_id() {
    let mut client = common::test_server().await;

    let err = client
        .remove_volume(RemoveVolumeRequest { id: String::new() })
        .await
        .expect_err("empty ID must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// publish / subscribe — communication boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_rejects_empty_topic() {
    let mut client = common::test_server().await;

    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: String::new(),
            payload: vec![],
        })
        .await
        .expect_err("empty topic must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn publish_rejects_topic_with_leading_dot() {
    let mut client = common::test_server().await;

    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: ".events".into(),
            payload: vec![],
        })
        .await
        .expect_err("leading-dot topic must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn publish_rejects_oversized_payload() {
    let mut client = common::test_server().await;

    // 2 MiB — well over the 1 MiB cap. This is also a DoS defence: the
    // validator rejects before the broker allocates buffers.
    let payload = vec![0u8; 2 * 1024 * 1024];

    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: "events".into(),
            payload,
        })
        .await
        .expect_err("oversized payload must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn publish_with_valid_inputs_for_unregistered_sandbox_returns_not_found() {
    let mut client = common::test_server().await;

    // Sanity check: a well-formed but unregistered sandbox reaches the
    // broker, which returns SandboxNotFound -> Status::not_found. If the
    // validator wrongly rejected this we'd see InvalidArgument instead.
    let err = client
        .publish(PublishRequest {
            sandbox_id: "deadbeef".into(),
            topic: "agent.events".into(),
            payload: b"hello".to_vec(),
        })
        .await
        .expect_err("publish for unregistered sandbox");

    assert_eq!(
        err.code(),
        Code::NotFound,
        "valid inputs must reach the broker; an unknown sandbox surfaces as NotFound"
    );
}

#[tokio::test]
async fn subscribe_rejects_empty_topic() {
    let mut client = common::test_server().await;

    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: "deadbeef".into(),
            topic: String::new(),
        })
        .await
        .expect_err("empty topic must be rejected");

    assert_eq!(err.code(), Code::InvalidArgument);
}
