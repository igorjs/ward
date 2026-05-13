// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the cross-sandbox communication RPCs.
//!
//! Two layers of coverage:
//!
//!   1. **Validation boundary.** Malformed requests must return
//!      Code::InvalidArgument before reaching the broker — otherwise
//!      "this feature is missing" would mask "your input was bad" and
//!      SDKs would have no way to distinguish.
//!
//!   2. **Broker behaviour over the wire.** Publish/Subscribe between
//!      same-group sandboxes, cross-group denial, Deny-policy enforcement,
//!      audit-log retrieval, and lifecycle cleanup on sandbox removal.
//!
//! Style: BDD names, AAA bodies. Each test starts a fresh in-process
//! server via `common::test_server`, so they are hermetic.

mod common;

use tonic::Code;

use ward_core::pb::{
    CommunicationMode as PbCommunicationMode, CommunicationPolicy as PbCommunicationPolicy,
    CreateSandboxRequest, GetCommunicationLogRequest, PublishRequest, RemoveSandboxRequest,
    SubscribeRequest,
};

// ---------------------------------------------------------------------------
// Helpers: build CreateSandboxRequests with explicit comms policies.
// ---------------------------------------------------------------------------

fn group_req(image: &str, group: &str) -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: image.to_string(),
        comms: Some(PbCommunicationPolicy {
            mode: PbCommunicationMode::Group as i32,
            group: group.to_string(),
        }),
        ..Default::default()
    }
}

fn deny_req(image: &str) -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: image.to_string(),
        comms: Some(PbCommunicationPolicy {
            mode: PbCommunicationMode::Deny as i32,
            group: String::new(),
        }),
        ..Default::default()
    }
}

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
    // Arrange: leading-dot topics (".events") are ambiguous for routing
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
// subscribe — validation boundary
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

// ---------------------------------------------------------------------------
// Unregistered sandbox → NotFound (translates from broker's SandboxNotFound)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_unregistered_sandbox_when_publish_then_not_found() {
    // Arrange: well-formed UUID but no sandbox with that id was created.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
            topic: "events".into(),
            payload: b"x".to_vec(),
        })
        .await
        .expect_err("unregistered publisher");

    // Assert: NotFound. Locks in the ApiError -> Status mapping.
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_unregistered_sandbox_when_subscribe_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
            topic: "events".into(),
        })
        .await
        .expect_err("unregistered subscriber");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

// ---------------------------------------------------------------------------
// Publish + Subscribe end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_two_same_group_sandboxes_when_publish_then_subscriber_receives_message() {
    // Arrange: alice + bob both in group "team". Bob subscribes; alice publishes.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(group_req("alpine:1", "team"))
        .await
        .unwrap()
        .into_inner();
    let bob = client
        .create_sandbox(group_req("alpine:2", "team"))
        .await
        .unwrap()
        .into_inner();

    let mut stream = client
        .subscribe(SubscribeRequest {
            sandbox_id: bob.id.clone(),
            topic: "events".into(),
        })
        .await
        .expect("subscribe")
        .into_inner();

    // Act
    client
        .publish(PublishRequest {
            sandbox_id: alice.id.clone(),
            topic: "events".into(),
            payload: b"hello".to_vec(),
        })
        .await
        .expect("publish");

    // Assert: bob's subscriber stream yields exactly the message alice sent.
    let msg = stream
        .message()
        .await
        .expect("message")
        .expect("first event present");
    assert_eq!(msg.from_sandbox, alice.id);
    assert_eq!(msg.topic, "events");
    assert_eq!(msg.payload, b"hello".to_vec());
}

#[tokio::test]
async fn given_different_group_sandboxes_when_publish_then_subscriber_receives_nothing() {
    // Arrange: alice in "team-a", bob in "team-b". Group policy means
    // they can't see each other's traffic.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(group_req("alpine:1", "team-a"))
        .await
        .unwrap()
        .into_inner();
    let bob = client
        .create_sandbox(group_req("alpine:2", "team-b"))
        .await
        .unwrap()
        .into_inner();

    let mut stream = client
        .subscribe(SubscribeRequest {
            sandbox_id: bob.id,
            topic: "events".into(),
        })
        .await
        .expect("subscribe")
        .into_inner();

    // Act
    client
        .publish(PublishRequest {
            sandbox_id: alice.id,
            topic: "events".into(),
            payload: b"hello".to_vec(),
        })
        .await
        .expect("publish");

    // Assert: nothing arrives within a generous timeout. tokio's
    // `timeout` returns Err on elapsed; that's our success signal here.
    let result =
        tokio::time::timeout(std::time::Duration::from_millis(100), stream.message()).await;
    assert!(
        result.is_err(),
        "subscriber must not receive cross-group traffic"
    );
}

// ---------------------------------------------------------------------------
// Deny policy enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_deny_sandbox_when_publish_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(deny_req("alpine"))
        .await
        .unwrap()
        .into_inner();

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: alice.id,
            topic: "events".into(),
            payload: b"x".to_vec(),
        })
        .await
        .expect_err("Deny publisher");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("Deny"),
        "should mention Deny: {}",
        err.message()
    );
}

#[tokio::test]
async fn given_deny_sandbox_when_subscribe_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(deny_req("alpine"))
        .await
        .unwrap()
        .into_inner();

    // Act
    let err = client
        .subscribe(SubscribeRequest {
            sandbox_id: alice.id,
            topic: "events".into(),
        })
        .await
        .expect_err("Deny subscriber");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

// ---------------------------------------------------------------------------
// GetCommunicationLog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_publisher_when_get_communication_log_then_records_recent_entries() {
    // Arrange: alice publishes twice; her audit log should show both
    // entries even though no one was subscribed.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(group_req("alpine", "team"))
        .await
        .unwrap()
        .into_inner();

    client
        .publish(PublishRequest {
            sandbox_id: alice.id.clone(),
            topic: "events.a".into(),
            payload: b"first".to_vec(),
        })
        .await
        .unwrap();
    client
        .publish(PublishRequest {
            sandbox_id: alice.id.clone(),
            topic: "events.b".into(),
            payload: b"second".to_vec(),
        })
        .await
        .unwrap();

    // Act
    let log = client
        .get_communication_log(GetCommunicationLogRequest {
            sandbox_id: alice.id.clone(),
        })
        .await
        .expect("get_communication_log")
        .into_inner();

    // Assert: two entries, both allowed, both from alice, distinct topics.
    assert_eq!(log.entries.len(), 2);
    let topics: Vec<&str> = log.entries.iter().map(|e| e.topic.as_str()).collect();
    assert!(topics.contains(&"events.a"));
    assert!(topics.contains(&"events.b"));
    assert!(log.entries.iter().all(|e| e.allowed));
    assert!(log.entries.iter().all(|e| e.from_sandbox == alice.id));
}

#[tokio::test]
async fn given_deny_publish_when_get_communication_log_then_records_attempt() {
    // Arrange: even denied attempts are logged so operators can audit.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(deny_req("alpine"))
        .await
        .unwrap()
        .into_inner();

    let _ = client
        .publish(PublishRequest {
            sandbox_id: alice.id.clone(),
            topic: "events".into(),
            payload: b"x".to_vec(),
        })
        .await
        .expect_err("Deny");

    // Act
    let log = client
        .get_communication_log(GetCommunicationLogRequest {
            sandbox_id: alice.id,
        })
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert_eq!(log.entries.len(), 1);
    assert!(!log.entries[0].allowed);
    assert_eq!(log.entries[0].subscriber_count, 0);
}

#[tokio::test]
async fn given_no_activity_when_get_communication_log_then_returns_empty() {
    // Arrange: brand-new sandbox with no publish or subscribe yet.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(group_req("alpine", "team"))
        .await
        .unwrap()
        .into_inner();

    // Act
    let log = client
        .get_communication_log(GetCommunicationLogRequest {
            sandbox_id: alice.id,
        })
        .await
        .unwrap()
        .into_inner();

    // Assert
    assert!(log.entries.is_empty());
}

// ---------------------------------------------------------------------------
// Lifecycle: sandbox removal cleans up broker state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_sandbox_removed_when_publish_with_old_id_then_not_found() {
    // Arrange: create alice, remove her, then try to publish as her.
    // SandboxManager.remove() deregisters from the broker, so the
    // broker no longer recognises the id.
    let mut client = common::test_server().await;
    let alice = client
        .create_sandbox(group_req("alpine", "team"))
        .await
        .unwrap()
        .into_inner();
    client
        .remove_sandbox(RemoveSandboxRequest {
            id: alice.id.clone(),
        })
        .await
        .unwrap();

    // Act
    let err = client
        .publish(PublishRequest {
            sandbox_id: alice.id,
            topic: "events".into(),
            payload: b"x".to_vec(),
        })
        .await
        .expect_err("publish after remove");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}
