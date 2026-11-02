// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! gRPC round trip latency of a cheap RPC (`GetHealth`) against an
//! in-process tonic server, over a Unix socket and over TCP loopback.

mod common;

use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::runtime::Runtime;
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tonic::transport::{Channel, Endpoint, Server, Uri};
use tower::service_fn;

use ward_core::grpc::WardGrpcServer;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::ward_server::WardServer;

const SOCKET_NAME: &str = "ward.sock";
// tonic needs some URI even when the connector ignores it.
const UNIX_PLACEHOLDER_URI: &str = "http://[::1]:50051";

fn service(data_dir: &Path) -> WardServer<WardGrpcServer> {
    let (sandboxes, volumes) = common::managers(data_dir);
    WardServer::new(WardGrpcServer::new(sandboxes, volumes))
}

async fn connect_tcp(data_dir: &Path) -> WardClient<Channel> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind tcp");
    let addr = listener.local_addr().expect("local addr");
    let svc = service(data_dir);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect tcp");
    WardClient::new(channel)
}

async fn connect_unix(data_dir: &Path) -> WardClient<Channel> {
    let socket = data_dir.join(SOCKET_NAME);
    let listener = UnixListener::bind(&socket).expect("bind unix");
    let svc = service(data_dir);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await
    });
    let channel = Endpoint::try_from(UNIX_PLACEHOLDER_URI)
        .expect("endpoint")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move { UnixStream::connect(socket).await.map(TokioIo::new) }
        }))
        .await
        .expect("connect unix");
    WardClient::new(channel)
}

fn bench_round_trip(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let tcp_dir = tempfile::tempdir().expect("tempdir");
    let unix_dir = tempfile::tempdir().expect("tempdir");
    let tcp = rt.block_on(connect_tcp(tcp_dir.path()));
    let unix = rt.block_on(connect_unix(unix_dir.path()));

    let mut group = c.benchmark_group("grpc_get_health_round_trip");
    group.bench_function("unix_socket", |b| {
        b.to_async(&rt)
            .iter(|| async { unix.clone().get_health(()).await.expect("health") });
    });
    group.bench_function("tcp_loopback", |b| {
        b.to_async(&rt)
            .iter(|| async { tcp.clone().get_health(()).await.expect("health") });
    });
    group.finish();
}

criterion_group!(benches, bench_round_trip);
criterion_main!(benches);
