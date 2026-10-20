// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the `Attach` stream against a real wardd subprocess.
//!
//! Style: BDD names with AAA bodies, one scenario per test. The stub backend
//! runs no real process, so the process behaviour (stdin EOF, signals,
//! resize, exit codes, detach) is covered by the host-process tests in
//! `ward-core`; these tests cover the wire and the daemon.

mod common;

use std::time::Duration;

use common::Daemon;
use tokio_stream::wrappers::ReceiverStream;
#[cfg(not(feature = "krunvm"))]
use tonic::Code;
use tonic::metadata::MetadataValue;
#[cfg(not(feature = "krunvm"))]
use ward_core::pb::AttachStdin;
use ward_core::pb::attach_message::Kind;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{AttachMessage, AttachMode, AttachStart, CreateSandboxRequest, SandboxInfo};

const TOKEN_HEADER: &str = "x-ward-sandbox-token";
const STREAM_DEADLINE: Duration = Duration::from_secs(30);
const FEED_CAPACITY: usize = 4;

type Client = WardClient<tonic::transport::Channel>;

async fn create(client: &mut Client, command: &[&str]) -> SandboxInfo {
    let request = CreateSandboxRequest {
        image: "alpine:latest".into(),
        command: command.iter().map(ToString::to_string).collect(),
        ..Default::default()
    };
    client
        .create_sandbox(request)
        .await
        .expect("create")
        .into_inner()
}

fn start_message(start: AttachStart) -> AttachMessage {
    AttachMessage {
        kind: Some(Kind::Start(start)),
    }
}

fn main_start(id: &str) -> AttachStart {
    AttachStart {
        sandbox_id: id.into(),
        mode: AttachMode::MainProcess as i32,
        ..Default::default()
    }
}

/// Open an Attach stream whose first message is `start`. The sender stays
/// open so the stream is not half-closed.
async fn attach(
    client: &mut Client,
    token: Option<&str>,
    start: AttachStart,
) -> Result<
    (
        tokio::sync::mpsc::Sender<AttachMessage>,
        tonic::Streaming<AttachMessage>,
    ),
    tonic::Status,
> {
    let (feed, inbound) = tokio::sync::mpsc::channel(FEED_CAPACITY);
    feed.send(start_message(start)).await.expect("queue start");
    let mut request = tonic::Request::new(ReceiverStream::new(inbound));
    if let Some(token) = token {
        let value = MetadataValue::try_from(token).expect("ascii token");
        request.metadata_mut().insert(TOKEN_HEADER, value);
    }
    let replies = client.attach(request).await?.into_inner();
    Ok((feed, replies))
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_wrong_missing_or_empty_token_when_attach_then_permission_denied() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client, &["echo"]).await;

    for token in [Some("wrong"), None, Some("")] {
        // Act
        let result = attach(&mut client, token, main_start(&sandbox.id)).await;

        // Assert
        let code = result.err().map(|status| status.code());
        assert_eq!(code, Some(Code::PermissionDenied), "token {token:?}");
    }
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_start_with_user_when_attach_then_unimplemented() {
    // Arrange
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client, &["echo"]).await;
    let start = AttachStart {
        user: "1000".into(),
        ..main_start(&sandbox.id)
    };

    // Act
    let result = attach(&mut client, Some(&sandbox.token), start).await;

    // Assert
    assert_eq!(
        result.err().map(|status| status.code()),
        Some(Code::Unimplemented)
    );
}

#[cfg(not(feature = "krunvm"))]
#[tokio::test]
async fn given_output_before_attach_when_replayed_with_follow_then_lines_and_exit_arrive() {
    // Arrange: the stub prints one line for the command, then exits 0.
    let daemon = Daemon::spawn();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client, &["echo"]).await;
    let start = AttachStart {
        replay: true,
        follow: true,
        ..main_start(&sandbox.id)
    };
    let (feed, mut replies) = attach(&mut client, Some(&sandbox.token), start)
        .await
        .expect("attach");

    // Act: stdin on a read-only stream is ignored.
    let stdin = Kind::Stdin(AttachStdin {
        data: b"ignored".to_vec(),
    });
    let _ = feed.send(AttachMessage { kind: Some(stdin) }).await;
    let mut text = Vec::new();
    let mut exit_code = None;
    let read = async {
        while let Some(reply) = replies.message().await.expect("reply") {
            match reply.kind {
                Some(Kind::Data(data)) => text.extend(data.data),
                Some(Kind::Exit(exit)) => exit_code = Some(exit.exit_code),
                _ => {}
            }
        }
    };
    tokio::time::timeout(STREAM_DEADLINE, read)
        .await
        .expect("stream ends");

    // Assert
    assert_eq!(String::from_utf8_lossy(&text), "stub: echo\n");
    assert_eq!(exit_code, Some(0));
}

#[cfg(feature = "krunvm")]
#[tokio::test]
async fn given_attached_client_when_daemon_stops_then_client_gets_an_error() {
    // Arrange
    if std::env::var("WARD_E2E_VM").as_deref() != Ok("1") {
        eprintln!("skipped: set WARD_E2E_VM=1 to run real-VM tests");
        return;
    }
    let mut daemon = Daemon::spawn_vm();
    let mut client = common::connect(&daemon.socket).await;
    let sandbox = create(&mut client, &["sleep", "60"]).await;
    let (_feed, mut replies) = attach(&mut client, Some(&sandbox.token), main_start(&sandbox.id))
        .await
        .expect("attach");

    // Act
    daemon.kill_hard();
    let ended = tokio::time::timeout(STREAM_DEADLINE, replies.message())
        .await
        .expect("stream ends");

    // Assert
    assert!(ended.is_err(), "expected an error, got {ended:?}");
}
