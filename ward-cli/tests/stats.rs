// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward stats` against a fake daemon that answers from canned readings, so
//! the CLI's own arithmetic, layout and error wording are tested without a
//! guest.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::{
    GetSandboxRequest, ListSandboxesResponse, SandboxInfo, SandboxStatus, StatsRequest,
    StatsResponse,
};

/// Its second reading fails, as a sandbox that stopped between two readings.
const STOPS_MIDWAY: &str = "gone";
/// Not a microVM, so the daemon refuses to read it.
const NO_AGENT: &str = "plain";
const STOPPED: &str = "off";
const SANDBOXES: [&str; 4] = ["web1", NO_AGENT, STOPS_MIDWAY, STOPPED];
const MIB: u64 = 1024 * 1024;
const NANOS_PER_READING: u64 = 500_000_000;
/// Each reading is stamped one second after the one before.
const SECONDS_PER_READING: i64 = 1;

#[derive(Clone, Default)]
struct FakeWard {
    reads: Arc<Mutex<HashMap<String, u64>>>,
}

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

fn info(id: &str) -> SandboxInfo {
    let status = if id == STOPPED {
        SandboxStatus::Stopped
    } else {
        SandboxStatus::Running
    };
    SandboxInfo {
        status: status as i32,
        name: id.to_string(),
        id: id.to_string(),
        ..Default::default()
    }
}

fn get_sandbox(request: GetSandboxRequest) -> Result<SandboxInfo, tonic::Status> {
    Ok(info(&request.id))
}

fn list_sandboxes(_: ()) -> Result<ListSandboxesResponse, tonic::Status> {
    Ok(ListSandboxesResponse {
        sandboxes: SANDBOXES.map(info).to_vec(),
    })
}

fn stats(fake: &FakeWard, request: StatsRequest) -> Result<StatsResponse, tonic::Status> {
    let id = request.sandbox_id;
    let mut reads = fake.reads.lock().unwrap();
    let count = reads.entry(id.clone()).or_default();
    *count += 1;
    let count = *count;
    if id == NO_AGENT || id == STOPPED {
        return Err(tonic::Status::invalid_argument(
            "this needs a sandbox booted as a microVM",
        ));
    }
    if id == STOPS_MIDWAY && count > 1 {
        return Err(tonic::Status::invalid_argument(format!(
            "Container {id} is not running"
        )));
    }
    Ok(StatsResponse {
        cpu_nanos: NANOS_PER_READING * count,
        online_cpus: 2,
        mem_used_bytes: 16 * MIB,
        mem_limit_bytes: 64 * MIB,
        pids: 4,
        read_at: Some(prost_types::Timestamp {
            seconds: SECONDS_PER_READING * count as i64,
            nanos: 0,
        }),
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
        let fake = self.clone();
        Box::pin(async move {
            Ok(match request.uri().path() {
                "/ward.v1.Ward/ListSandboxes" => serve(list_sandboxes, request).await,
                "/ward.v1.Ward/GetSandbox" => serve(get_sandbox, request).await,
                "/ward.v1.Ward/Stats" => serve(|r| stats(&fake, r), request).await,
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
        let tokens: HashMap<_, _> = SANDBOXES.iter().map(|id| (*id, "t")).collect();
        std::fs::write(
            dir.path().join("tokens.json"),
            serde_json::to_string(&tokens).unwrap(),
        )
        .unwrap();
        let incoming = UnixListenerStream::new(UnixListener::bind(&socket).unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(FakeWard::default())
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

/// The `--format` fields that vary between the fake sandboxes.
const ROW_FORMAT: &str = "{{.Name}} {{.CPUPerc}} {{.PIDs}}";

async fn rows(fixture: &Fixture, args: &[&str]) -> Vec<String> {
    let out = fixture
        .ward(&[&["stats", "--no-stream", "--format", ROW_FORMAT], args].concat())
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout).lines().map(String::from).collect()
}

#[tokio::test]
async fn given_two_readings_when_stats_then_the_row_holds_the_rate_between_them() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["stats", "--no-stream", "web1"]).await;

    // Assert
    let stdout = text(&out.stdout);
    let row = stdout.lines().nth(1).unwrap().split_whitespace();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(stdout.starts_with("CONTAINER ID"), "{stdout}");
    // Half a second of CPU over one second, and 16 of 64 MiB.
    assert_eq!(
        row.collect::<Vec<_>>().join(" "),
        "web1 web1 50.00% 16MiB / 64MiB 25.00% -- -- 4"
    );
}

#[tokio::test]
async fn given_a_sandbox_that_stops_after_the_first_reading_when_stats_named_then_dashes() {
    let fixture = Fixture::start().await;

    assert_eq!(rows(&fixture, &[STOPS_MIDWAY]).await, ["gone -- --"]);
}

#[tokio::test]
async fn given_no_names_when_stats_then_sandboxes_that_cannot_be_read_are_skipped() {
    let fixture = Fixture::start().await;

    assert_eq!(rows(&fixture, &[]).await, ["web1 50.00% 4"]);
}

#[tokio::test]
async fn given_all_when_stats_then_sandboxes_that_cannot_be_read_show_dashes() {
    let fixture = Fixture::start().await;

    assert_eq!(
        rows(&fixture, &["-a"]).await,
        ["web1 50.00% 4", "plain -- --", "gone -- --", "off -- --"]
    );
}

#[tokio::test]
async fn given_a_sandbox_that_is_not_a_microvm_when_stats_names_it_then_the_error_is_reported() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["stats", "--no-stream", NO_AGENT]).await;

    // Assert
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("microVM"),
        "{}",
        text(&out.stderr)
    );
}
