// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ListImages` and `PullImage` over a real gRPC connection.

mod common;

use ward_core::pb::PullImageRequest;

const IMAGE: &str = "alpine:3";

#[tokio::test]
async fn given_a_pull_when_it_streams_then_docker_lines_arrive_and_the_image_is_listed() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let mut stream = client
        .pull_image(PullImageRequest {
            reference: IMAGE.to_string(),
        })
        .await
        .expect("pull starts")
        .into_inner();
    let mut statuses = Vec::new();
    while let Some(line) = stream.message().await.expect("stream") {
        statuses.push(line.status);
    }
    let images = client
        .list_images(())
        .await
        .expect("list")
        .into_inner()
        .images;

    // Assert
    assert_eq!(
        statuses.first().map(String::as_str),
        Some("Pulling from library/alpine")
    );
    assert_eq!(
        statuses.last().map(String::as_str),
        Some("Status: Downloaded newer image for alpine:3")
    );
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].reference, IMAGE);
}

#[tokio::test]
async fn given_an_invalid_reference_when_pulled_then_the_stream_fails_with_invalid_argument() {
    // Arrange
    let mut client = common::test_server().await;

    // Act
    let mut stream = client
        .pull_image(PullImageRequest {
            reference: "Not A Ref".to_string(),
        })
        .await
        .expect("pull starts")
        .into_inner();
    let error = stream.message().await.expect_err("rejected");

    // Assert
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(error.message().contains("invalid reference"), "{error}");
}
