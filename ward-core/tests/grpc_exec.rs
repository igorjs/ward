// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Integration tests for the Exec / Run / StreamOutput RPCs.
//!
//! Real VM execution is gated behind the krunvm cargo feature; in stub
//! mode the backend returns a synthetic ProcessHandle with a fresh UUID
//! pid. Those tests still catch every wiring bug between the validator,
//! the manager, the gRPC layer, and the backend — they just don't run
//! a real command inside the sandbox.
//!
//! When the krunvm feature lands, this file gains positive output-
//! streaming scenarios. The negative-path contracts here continue to hold.

mod common;

use tonic::Code;

use ward_core::pb::{CreateSandboxRequest, ExecRequest, RunRequest, StreamOutputRequest};

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_sandbox_when_exec_echo_then_returns_pid_and_running_status() {
    // Arrange: an existing sandbox is the precondition for exec.
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
    let resp = client
        .exec(ExecRequest {
            sandbox_id: s.id.clone(),
            command: vec!["echo".into(), "hello".into()],
            working_dir: String::new(),
            env: Default::default(),
        })
        .await
        .expect("exec");
    let info = resp.into_inner();

    // Assert: stub returns a real-shaped ProcessInfo.
    assert_eq!(info.pid.len(), 36);
    assert_eq!(info.sandbox_id, s.id);
    assert_eq!(info.status, "running");
}

#[tokio::test]
async fn given_empty_command_when_exec_then_invalid_argument() {
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
    let err = client
        .exec(ExecRequest {
            sandbox_id: s.id,
            command: vec![],
            working_dir: String::new(),
            env: Default::default(),
        })
        .await
        .expect_err("empty command");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_unknown_sandbox_when_exec_then_not_found() {
    // Arrange: well-formed UUID with no matching sandbox.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .exec(ExecRequest {
            sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
            command: vec!["echo".into()],
            working_dir: String::new(),
            env: Default::default(),
        })
        .await
        .expect_err("unknown sandbox");

    // Assert: NotFound (not Internal). Regression guard for the
    // backend_err helper in SandboxManager.
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn given_malformed_sandbox_id_when_exec_then_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .exec(ExecRequest {
            sandbox_id: "not-a-uuid-zzzz".into(),
            command: vec!["echo".into()],
            working_dir: String::new(),
            env: Default::default(),
        })
        .await
        .expect_err("malformed id");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_exec_with_working_dir_when_request_succeeds_then_does_not_leak() {
    // Arrange: working_dir is accepted but not echoed in ProcessInfo
    // (which only carries identity fields). This is a regression guard:
    // SDKs should not depend on reading working_dir back from the response.
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
    let resp = client
        .exec(ExecRequest {
            sandbox_id: s.id,
            command: vec!["pwd".into()],
            working_dir: "/work".into(),
            env: Default::default(),
        })
        .await
        .expect("exec")
        .into_inner();

    // Assert: pid + sandbox_id + status, nothing else.
    assert!(!resp.pid.is_empty());
    assert_eq!(resp.status, "running");
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_existing_sandbox_when_run_python_then_returns_pid() {
    // Arrange
    let mut client = common::test_server().await;
    let s = client
        .create_sandbox(CreateSandboxRequest {
            image: "python:3.12-slim".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    // Act
    let resp = client
        .run(RunRequest {
            sandbox_id: s.id.clone(),
            language: "python".into(),
            code: "print('hi')".into(),
        })
        .await
        .expect("run");
    let info = resp.into_inner();

    // Assert
    assert_eq!(info.pid.len(), 36);
    assert_eq!(info.sandbox_id, s.id);
    assert_eq!(info.status, "running");
}

#[tokio::test]
async fn given_unsupported_language_when_run_then_invalid_argument() {
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
    let err = client
        .run(RunRequest {
            sandbox_id: s.id,
            language: "cobol".into(),
            code: "DISPLAY 'hi'".into(),
        })
        .await
        .expect_err("unsupported language");

    // Assert: surfaces as InvalidArgument from the runtime-table lookup.
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().contains("unsupported language"),
        "got: {}",
        err.message(),
    );
}

#[tokio::test]
async fn given_invalid_language_name_when_run_then_invalid_argument() {
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

    // Act: dash in the language name fails the language_name validator
    // BEFORE the runtime-table lookup. This catches the validator early
    // so unknown languages don't reach the routing logic.
    let err = client
        .run(RunRequest {
            sandbox_id: s.id,
            language: "py-thon".into(),
            code: "print('hi')".into(),
        })
        .await
        .expect_err("invalid language name");

    // Assert
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn given_unknown_sandbox_when_run_then_not_found() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let err = client
        .run(RunRequest {
            sandbox_id: "00000000-0000-0000-0000-000000000000".into(),
            language: "python".into(),
            code: "print('hi')".into(),
        })
        .await
        .expect_err("unknown sandbox");

    // Assert
    assert_eq!(err.code(), Code::NotFound);
}

// ---------------------------------------------------------------------------
// StreamOutput
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_valid_stream_request_when_call_then_unimplemented_not_invalid() {
    // Arrange: StreamOutput is not yet implemented in the daemon. This
    // test locks in the contract that VALID inputs reach the stub —
    // when streaming lands, the same test will need to be updated to
    // expect a working stream rather than Unimplemented.
    let mut client = common::test_server().await;

    // Act
    let err = client
        .stream_output(StreamOutputRequest {
            sandbox_id: "deadbeef".into(),
            pid: "deadbeef".into(),
        })
        .await
        .expect_err("stream stub returns unimplemented");

    // Assert
    assert_eq!(err.code(), Code::Unimplemented);
}
