// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the sandbox capability token: wire round-trip on
//! `CreateSandbox`'s response, the `GetSandbox`/`ListSandboxes`
//! no-token-required regression pin, and enforcement of the token on every
//! sandbox-scoped RPC handler.
//!
//! Style: BDD names with AAA bodies. Every test starts a fresh in-process
//! server via `common::test_server`, so they are hermetic.

mod common;

use tonic::{Request, Status};

use ward_core::pb::{
    CreateSandboxRequest, CreateSnapshotRequest, ExecRequest, GetCommunicationLogRequest,
    GetEgressLogRequest, GetSandboxRequest, KillProcessRequest, ListSnapshotsRequest,
    PublishRequest, RemoveSandboxRequest, RestoreSnapshotRequest, RunRequest, StreamOutputRequest,
    SubscribeRequest, WriteStdinRequest,
};

#[tokio::test]
async fn given_no_token_when_get_sandbox_then_still_succeeds() {
    // Arrange: a sandbox created with no caller-supplied metadata at all.
    let mut client = common::test_server().await;
    let created = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    // Act: GetSandbox carries no `x-ward-sandbox-token` metadata.
    let resp = client
        .get_sandbox(GetSandboxRequest {
            id: created.id.clone(),
        })
        .await
        .expect("get_sandbox should succeed without a token");

    // Assert: GetSandbox never echoes the real token back, regardless of
    // whether a token was presented.
    let info = resp.into_inner();
    assert_eq!(info.id, created.id);
    assert!(
        info.token.is_empty(),
        "GetSandbox must never expose the capability token"
    );
}

#[tokio::test]
async fn given_create_sandbox_when_response_returned_then_wire_token_matches_domain_token() {
    // Arrange
    let mut client = common::test_server().await;

    // Act: CreateSandbox needs no token itself.
    let created = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    // Assert: the response carries a real, non-empty capability token.
    assert!(
        !created.token.is_empty(),
        "CreateSandbox response must carry the minted token"
    );

    // Assert: that exact token, presented on a subsequent authorize-gated
    // call against the same sandbox id, is accepted. ListSnapshots is a
    // convenient authorize-gated call here: it needs no additional setup
    // beyond a valid sandbox id to succeed.
    let request = common::with_token(
        ListSnapshotsRequest {
            sandbox_id: created.id.clone(),
        },
        &created.token,
    );
    client
        .list_snapshots(request)
        .await
        .expect("list_snapshots with the sandbox's real token must succeed");
}

#[tokio::test]
async fn given_no_token_when_kill_process_then_permission_denied() {
    // Arrange: a real sandbox, no token metadata on the follow-up call.
    let mut client = common::test_server().await;
    let sandbox_id = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id;

    // Act
    let status = client
        .kill_process(Request::new(KillProcessRequest {
            sandbox_id,
            pid: "11111111-1111-1111-1111-111111111111".into(),
        }))
        .await
        .expect_err("kill_process without a token must be rejected");

    // Assert
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn given_wrong_token_when_remove_sandbox_then_permission_denied() {
    // Arrange: a real sandbox, a garbage token on the follow-up call.
    let mut client = common::test_server().await;
    let sandbox_id = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id;

    // Act
    let request = common::with_token(
        RemoveSandboxRequest { id: sandbox_id },
        "not-the-real-token",
    );
    let status = client
        .remove_sandbox(request)
        .await
        .expect_err("remove_sandbox with a wrong token must be rejected");

    // Assert
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn given_correct_token_when_kill_process_then_ok_or_not_found_for_missing_process() {
    // Arrange: a real sandbox and its real token, but a process id that
    // was never started.
    let mut client = common::test_server().await;
    let created = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    // Act
    let request = common::with_token(
        KillProcessRequest {
            sandbox_id: created.id,
            pid: "11111111-1111-1111-1111-111111111111".into(),
        },
        &created.token,
    );
    let status = client
        .kill_process(request)
        .await
        .expect_err("kill_process against a missing process must fail");

    // Assert: the token check must not swallow the real downstream error.
    assert_eq!(
        status.code(),
        tonic::Code::NotFound,
        "expected NotFound for a missing process, got {status:?}"
    );
}

#[tokio::test]
async fn given_second_caller_with_valid_sandbox_id_but_no_token_when_publish_then_permission_denied()
 {
    // Arrange: a real sandbox; the publish request itself is otherwise
    // valid (real sandbox id, valid topic and payload) so only the missing
    // token can cause the rejection.
    let mut client = common::test_server().await;
    let sandbox_id = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id;

    // Act
    let status = client
        .publish(Request::new(PublishRequest {
            sandbox_id,
            topic: "test-topic".into(),
            payload: b"hello".to_vec(),
        }))
        .await
        .expect_err("publish without a token must be rejected");

    // Assert
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn given_token_for_sandbox_a_when_used_against_sandbox_b_then_permission_denied() {
    // Arrange: two independent sandboxes, each with its own real token.
    let mut client = common::test_server().await;
    let sandbox_a = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create sandbox a")
        .into_inner();
    let sandbox_b = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create sandbox b")
        .into_inner();

    // Act: sandbox A's real token, presented against sandbox B's id.
    let request = common::with_token(RemoveSandboxRequest { id: sandbox_b.id }, &sandbox_a.token);
    let status = client
        .remove_sandbox(request)
        .await
        .expect_err("a token for a different sandbox must be rejected");

    // Assert
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
}

/// Table-driven: the remaining ten sandbox-scoped handlers not exercised by
/// the dedicated tests above. Each request carries a real sandbox id and
/// otherwise-valid fields, so a failure here can only be the missing
/// `x-ward-sandbox-token` metadata, proving the token check runs before any
/// downstream lookup.
#[tokio::test]
async fn given_no_token_when_calling_remaining_gated_handlers_then_all_permission_denied() {
    // Arrange
    let mut client = common::test_server().await;
    let sandbox_id = client
        .create_sandbox(CreateSandboxRequest {
            image: "alpine:latest".into(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id;

    let cases = [
        "exec",
        "run",
        "stream_output",
        "write_stdin",
        "create_snapshot",
        "restore_snapshot",
        "list_snapshots",
        "get_egress_log",
        "subscribe",
        "get_communication_log",
    ];

    for case in cases {
        // Act
        let status: Status = match case {
            "exec" => client
                .exec(Request::new(ExecRequest {
                    sandbox_id: sandbox_id.clone(),
                    command: vec!["true".into()],
                    ..Default::default()
                }))
                .await
                .expect_err("exec without a token must be rejected"),
            "run" => client
                .run(Request::new(RunRequest {
                    sandbox_id: sandbox_id.clone(),
                    language: "shell".into(),
                    code: "true".into(),
                }))
                .await
                .expect_err("run without a token must be rejected"),
            "stream_output" => client
                .stream_output(Request::new(StreamOutputRequest {
                    sandbox_id: sandbox_id.clone(),
                    pid: "11111111-1111-1111-1111-111111111111".into(),
                }))
                .await
                .expect_err("stream_output without a token must be rejected"),
            "write_stdin" => client
                .write_stdin(Request::new(WriteStdinRequest {
                    sandbox_id: sandbox_id.clone(),
                    pid: "11111111-1111-1111-1111-111111111111".into(),
                    data: vec![],
                }))
                .await
                .expect_err("write_stdin without a token must be rejected"),
            "create_snapshot" => client
                .create_snapshot(Request::new(CreateSnapshotRequest {
                    sandbox_id: sandbox_id.clone(),
                    label: "snap".into(),
                }))
                .await
                .expect_err("create_snapshot without a token must be rejected"),
            "restore_snapshot" => client
                .restore_snapshot(Request::new(RestoreSnapshotRequest {
                    sandbox_id: sandbox_id.clone(),
                    snapshot_id: "22222222-2222-2222-2222-222222222222".into(),
                }))
                .await
                .expect_err("restore_snapshot without a token must be rejected"),
            "list_snapshots" => client
                .list_snapshots(Request::new(ListSnapshotsRequest {
                    sandbox_id: sandbox_id.clone(),
                }))
                .await
                .expect_err("list_snapshots without a token must be rejected"),
            "get_egress_log" => client
                .get_egress_log(Request::new(GetEgressLogRequest {
                    sandbox_id: sandbox_id.clone(),
                }))
                .await
                .expect_err("get_egress_log without a token must be rejected"),
            "subscribe" => client
                .subscribe(Request::new(SubscribeRequest {
                    sandbox_id: sandbox_id.clone(),
                    topic: "test-topic".into(),
                }))
                .await
                .expect_err("subscribe without a token must be rejected"),
            "get_communication_log" => client
                .get_communication_log(Request::new(GetCommunicationLogRequest {
                    sandbox_id: sandbox_id.clone(),
                }))
                .await
                .expect_err("get_communication_log without a token must be rejected"),
            other => unreachable!("unhandled case: {other}"),
        };

        // Assert
        assert_eq!(
            status.code(),
            tonic::Code::PermissionDenied,
            "case {case} should return PermissionDenied, got {status:?}"
        );
    }
}
