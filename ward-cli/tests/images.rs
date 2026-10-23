// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward images` and `ward pull` against a fake daemon that answers from
//! canned data, so the CLI's own output is tested without a registry.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::{ImageInfo, ListImagesResponse, PullImageRequest, PullProgress};

const SIZE_BYTES: u64 = 7_800_000;
const REJECTED: &str = "Not A Ref";

#[derive(Clone, Default)]
struct FakeWard;

struct ListImages;

impl tonic::server::UnaryService<()> for ListImages {
    type Response = ListImagesResponse;
    type Future = std::future::Ready<Result<tonic::Response<ListImagesResponse>, tonic::Status>>;

    fn call(&mut self, _: tonic::Request<()>) -> Self::Future {
        let image = |reference: &str| ImageInfo {
            reference: reference.to_string(),
            digest: format!("sha256:{}", "cd".repeat(32)),
            size_bytes: SIZE_BYTES,
            created_at: Some(prost_types::Timestamp::default()),
        };
        std::future::ready(Ok(tonic::Response::new(ListImagesResponse {
            images: vec![image("alpine:3.20"), image("busybox")],
        })))
    }
}

struct PullImage;

impl tonic::server::ServerStreamingService<PullImageRequest> for PullImage {
    type Response = PullProgress;
    type ResponseStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<PullProgress, tonic::Status>>>;
    type Future = std::future::Ready<Result<tonic::Response<Self::ResponseStream>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<PullImageRequest>) -> Self::Future {
        let reference = request.into_inner().reference;
        let line = |id: &str, status: &str| {
            Ok(PullProgress {
                id: id.to_string(),
                status: status.to_string(),
            })
        };
        let mut lines = vec![line("latest", "Pulling from library/alpine")];
        if reference == REJECTED {
            lines.push(Err(tonic::Status::invalid_argument(
                "invalid reference format",
            )));
        } else {
            lines.push(line("4abcf2066143", "Pull complete"));
            lines.push(line(
                "",
                &format!("Status: Downloaded newer image for {reference}"),
            ));
        }
        std::future::ready(Ok(tonic::Response::new(tokio_stream::iter(lines))))
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
                "/ward.v1.Ward/ListImages" => {
                    let codec = tonic_prost::ProstCodec::<ListImagesResponse, ()>::default();
                    tonic::server::Grpc::new(codec)
                        .unary(ListImages, request)
                        .await
                }
                "/ward.v1.Ward/PullImage" => {
                    let codec =
                        tonic_prost::ProstCodec::<PullProgress, PullImageRequest>::default();
                    tonic::server::Grpc::new(codec)
                        .server_streaming(PullImage, request)
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
async fn given_cached_images_when_listed_then_the_table_has_docker_columns_and_a_size() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["images"]).await;

    // Assert
    let stdout = text(&out.stdout);
    let mut lines = stdout.lines();
    let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        ["REPOSITORY", "TAG", "IMAGE", "ID", "CREATED", "SIZE"]
    );
    let alpine: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(&alpine[..3], ["alpine", "3.20", "cdcdcdcdcdcd"]);
    assert_eq!(alpine.last(), Some(&"7.8MB"));
    assert!(out.status.success());
}

#[tokio::test]
async fn given_quiet_and_a_repository_when_listed_then_only_matching_ids_print() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["images", "-q", "busybox"]).await;

    // Assert
    assert_eq!(text(&out.stdout), "cdcdcdcdcdcd\n");
}

#[tokio::test]
async fn given_a_pull_when_it_streams_then_docker_lines_print_in_order() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["pull", "alpine"]).await;

    // Assert
    assert_eq!(
        text(&out.stdout),
        "latest: Pulling from library/alpine\n4abcf2066143: Pull complete\n\
         Status: Downloaded newer image for alpine\n"
    );
}

#[tokio::test]
async fn given_a_rejected_reference_when_pulled_then_it_fails_with_the_daemon_error() {
    // Arrange
    let fixture = Fixture::start().await;

    // Act
    let out = fixture.ward(&["pull", REJECTED]).await;

    // Assert
    assert!(text(&out.stderr).contains("Error response from daemon: invalid reference format"));
    assert!(!out.status.success());
}
