// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward events` against a fake daemon that answers from canned events, so the CLI's own output and error wording
//! are tested without a guest.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::{EventsRequest, SandboxEvent};

const EVENT_SECONDS: i64 = 1_792_663_200;

#[derive(Clone, Default)]
struct FakeWard;

/// An `Events` stream that replays canned events and then ends.
struct CannedEvents;

impl tonic::server::ServerStreamingService<EventsRequest> for CannedEvents {
    type Response = SandboxEvent;
    type ResponseStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<SandboxEvent, tonic::Status>>>;
    type Future = std::future::Ready<Result<tonic::Response<Self::ResponseStream>, tonic::Status>>;

    fn call(&mut self, _: tonic::Request<EventsRequest>) -> Self::Future {
        let event = |action: &str, dropped| SandboxEvent {
            action: action.into(),
            sandbox_id: "web1".into(),
            at: Some(prost_types::Timestamp {
                seconds: EVENT_SECONDS,
                nanos: 0,
            }),
            attributes: [("image", "alpine"), ("name", "web1")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
            dropped,
        };
        let events = vec![
            Ok(event("create", 0)),
            Ok(event("start", 0)),
            Ok(event("die", 2)),
        ];
        std::future::ready(Ok(tonic::Response::new(tokio_stream::iter(events))))
    }
}

impl NamedService for FakeWard {
    const NAME: &'static str = "ward.v1.Ward";
}

impl tower::Service<http::Request<Body>> for FakeWard {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        Box::pin(async move {
            Ok(match request.uri().path() {
                "/ward.v1.Ward/Events" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .server_streaming(CannedEvents, request)
                        .await
                }
                _ => tonic::Status::unimplemented("not faked").into_http(),
            })
        })
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    socket: PathBuf,
}

impl Fixture {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ward.sock");
        let incoming = UnixListenerStream::new(UnixListener::bind(&socket).unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(FakeWard)
                .serve_with_incoming(incoming),
        );
        Self { dir, socket }
    }

    async fn ward(&self, args: &[&str]) -> std::process::Output {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_ward"))
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .env("HOME", self.dir.path())
            .env("XDG_DATA_HOME", self.dir.path())
            .output()
            .await
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn given_a_filter_and_a_format_and_dropped_events_when_events_then_only_matches_print_and_the_drop_is_warned()
 {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture
        .ward(&[
            "events",
            "--filter",
            "event=die",
            "--format",
            "{{.Action}} {{.ID}}",
        ])
        .await;

    // Assert
    assert_eq!(text(&out.stdout), "die web1\n");
    assert!(text(&out.stderr).contains("2 events were dropped"));
}
