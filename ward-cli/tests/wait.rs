// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward wait` against a fake daemon that answers from canned exit codes, so
//! the CLI's own output and error wording are tested without a guest.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::{GetSandboxRequest, SandboxInfo, SandboxStatus, WaitRequest, WaitResponse};

const UNKNOWN: &str = "ghost";
const WEB1: &str = "web1";
const EXIT_CODE_OF_WEB1: i32 = 3;

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
    let id = request.id;
    if id == UNKNOWN {
        return Err(tonic::Status::not_found(format!("No such container: {id}")));
    }
    Ok(SandboxInfo {
        status: SandboxStatus::Running as i32,
        name: id.clone(),
        id,
        ..Default::default()
    })
}

fn wait(request: WaitRequest) -> Result<WaitResponse, tonic::Status> {
    let exit_code = if request.sandbox_id == WEB1 {
        EXIT_CODE_OF_WEB1
    } else {
        0
    };
    Ok(WaitResponse { exit_code })
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
                "/ward.v1.Ward/Wait" => serve(wait, request).await,
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
async fn given_sandboxes_with_codes_and_one_unknown_when_wait_then_codes_print_in_order_and_the_unknown_fails()
 {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["wait", WEB1, UNKNOWN, "web2"]).await;

    // Assert
    assert_eq!(text(&out.stdout), "3\n0\n");
    assert!(text(&out.stderr).contains("Error response from daemon: No such container: ghost"));
    assert!(!out.status.success());
}
