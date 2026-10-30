// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward egress suggest --apply` against a fake daemon that reports fixed
//! blocked destinations and echoes the hosts it is asked to allow.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::{
    AddEgressAllowRequest, AddEgressAllowResponse, BlockedDestination, BlockedEgressRequest,
    BlockedEgressResponse, GetSandboxRequest, SandboxInfo,
};

const DENY_SANDBOX: &str = "deny1";
const BLOCKED_NAME: &str = "pypi.org";
const BLOCKED_IP: &str = "203.0.113.9";
const DENY_MESSAGE: &str =
    "sandbox was created with egress deny; recreate it with --egress allowlist";

#[derive(Clone, Default)]
struct FakeWard;

/// A unary RPC answered by a closure.
struct Handler<F>(F);

impl<Req, Resp, F> tonic::server::UnaryService<Req> for Handler<F>
where
    F: FnMut(Req) -> Result<Resp, tonic::Status>,
{
    type Response = Resp;
    type Future = std::future::Ready<Result<tonic::Response<Resp>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<Req>) -> Self::Future {
        std::future::ready((self.0)(request.into_inner()).map(tonic::Response::new))
    }
}

fn get_sandbox(request: GetSandboxRequest) -> Result<SandboxInfo, tonic::Status> {
    Ok(SandboxInfo {
        name: request.id.clone(),
        id: request.id,
        ..Default::default()
    })
}

fn blocked(_: BlockedEgressRequest) -> Result<BlockedEgressResponse, tonic::Status> {
    let destination = |host: &str| BlockedDestination {
        host: host.into(),
        count: 1,
        last_seen_unix: 0,
    };
    Ok(BlockedEgressResponse {
        destinations: vec![destination(BLOCKED_NAME), destination(BLOCKED_IP)],
    })
}

fn add_allow(request: AddEgressAllowRequest) -> Result<AddEgressAllowResponse, tonic::Status> {
    if request.sandbox_id == DENY_SANDBOX {
        return Err(tonic::Status::invalid_argument(DENY_MESSAGE));
    }
    Ok(AddEgressAllowResponse {
        added: request.domains,
    })
}

async fn serve<Req, Resp, F>(handler: F, request: http::Request<Body>) -> http::Response<Body>
where
    Req: tonic_prost::prost::Message + Default + Send + Sync + 'static,
    Resp: tonic_prost::prost::Message + Send + Sync + 'static,
    F: FnMut(Req) -> Result<Resp, tonic::Status>,
{
    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
        .unary(Handler(handler), request)
        .await
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
                "/ward.v1.Ward/GetSandbox" => serve(get_sandbox, request).await,
                "/ward.v1.Ward/BlockedEgress" => serve(blocked, request).await,
                "/ward.v1.Ward/AddEgressAllow" => serve(add_allow, request).await,
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
async fn given_blocked_name_and_ip_when_apply_then_only_the_name_is_added_and_the_ip_is_reported() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture
        .ward(&["egress", "suggest", "web1", "--apply"])
        .await;

    // Assert
    assert!(out.status.success());
    assert_eq!(
        text(&out.stdout),
        "Added pypi.org to the egress allowlist of web1\n"
    );
    assert!(text(&out.stderr).contains(BLOCKED_IP));
}

#[tokio::test]
async fn given_deny_sandbox_when_apply_then_the_daemon_reason_is_shown_and_it_fails() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture
        .ward(&["egress", "suggest", DENY_SANDBOX, "--apply"])
        .await;

    // Assert
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains(DENY_MESSAGE));
}
