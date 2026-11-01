// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward egress suggest` against a stub daemon. The stub backend has no
//! policed network, so nothing is ever recorded; these tests cover the RPC
//! path, token checks and the `AddEgressAllow` mode rules. A real recording
//! and a live allowlist update are covered in `e2e_vm.rs`.

mod common;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tonic::Code;
use tonic::metadata::MetadataValue;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    AddEgressAllowRequest, CreateSandboxRequest, EgressMode, EgressPolicy, SandboxInfo,
};

const TOKEN_HEADER: &str = "x-ward-sandbox-token";
const HOST: &str = "pypi.org";

fn create_sandbox(daemon: &common::Daemon) -> String {
    let out = daemon
        .cli()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(out.status.success());
    std::str::from_utf8(&out.stdout).unwrap().trim().to_string()
}

#[test]
fn given_sandbox_with_nothing_blocked_when_suggest_then_prints_no_flags() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);

    // Act
    let assertion = daemon.cli().args(["egress", "suggest", &id]).assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("No blocked destinations"));
}

#[test]
fn given_nothing_blocked_when_suggest_apply_then_reports_nothing_to_add() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);

    // Act
    let assertion = daemon
        .cli()
        .args(["egress", "suggest", &id, "--apply"])
        .assert();

    // Assert
    assertion
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Nothing to add"));
}

#[test]
fn given_no_stored_token_when_suggest_then_denied() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let id = create_sandbox(&daemon);
    let store = daemon.socket.parent().unwrap().join("tokens.json");
    std::fs::write(&store, "{}").expect("clear token store");

    // Act
    let assertion = daemon.cli().args(["egress", "suggest", &id]).assert();

    // Assert
    assertion
        .failure()
        .stderr(predicate::str::contains("token does not match"));
}

type Client = WardClient<tonic::transport::Channel>;

async fn create_with_egress(client: &mut Client, mode: EgressMode) -> SandboxInfo {
    let request = CreateSandboxRequest {
        image: "alpine:latest".into(),
        command: vec!["sleep".into(), "600".into()],
        egress: Some(EgressPolicy {
            mode: mode as i32,
            domains: Vec::new(),
        }),
        ..Default::default()
    };
    client
        .create_sandbox(request)
        .await
        .expect("create")
        .into_inner()
}

async fn add_allow(
    client: &mut Client,
    sandbox: &SandboxInfo,
    token: &str,
) -> Result<Vec<String>, tonic::Status> {
    let mut request = tonic::Request::new(AddEgressAllowRequest {
        sandbox_id: sandbox.id.clone(),
        domains: vec![HOST.into()],
    });
    request
        .metadata_mut()
        .insert(TOKEN_HEADER, MetadataValue::try_from(token).expect("ascii"));
    client
        .add_egress_allow(request)
        .await
        .map(|reply| reply.into_inner().added)
}

#[tokio::test]
async fn given_wrong_or_empty_token_when_add_egress_allow_then_permission_denied() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create_with_egress(&mut client, EgressMode::Allowlist).await;

    // Act
    let wrong = add_allow(&mut client, &sandbox, "wrong").await;
    let empty = add_allow(&mut client, &sandbox, "").await;

    // Assert
    assert_eq!(wrong.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(empty.unwrap_err().code(), Code::PermissionDenied);
}

#[tokio::test]
async fn given_allowlist_sandbox_when_add_egress_allow_then_host_is_added_once() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create_with_egress(&mut client, EgressMode::Allowlist).await;

    // Act
    let first = add_allow(&mut client, &sandbox, &sandbox.token).await;
    let again = add_allow(&mut client, &sandbox, &sandbox.token).await;

    // Assert
    assert_eq!(first.unwrap(), [HOST]);
    assert!(again.unwrap().is_empty());
}

#[tokio::test]
async fn given_deny_sandbox_when_add_egress_allow_then_invalid_request_says_to_recreate() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create_with_egress(&mut client, EgressMode::Deny).await;

    // Act
    let status = add_allow(&mut client, &sandbox, &sandbox.token)
        .await
        .unwrap_err();

    // Assert
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "sandbox was created with egress deny; recreate it with --egress allowlist"
    );
}

#[tokio::test]
async fn given_open_sandbox_when_add_egress_allow_then_nothing_is_added() {
    // Arrange
    let daemon = common::Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create_with_egress(&mut client, EgressMode::Open).await;

    // Act
    let added = add_allow(&mut client, &sandbox, &sandbox.token).await;

    // Assert
    assert!(added.unwrap().is_empty());
}
