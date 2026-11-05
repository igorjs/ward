// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward image inspect`: a cached image as `docker image inspect` shapes it.

use anyhow::{Result, anyhow};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{ImageHistoryRequest, ImageHistoryResponse, ImageInfo};

use crate::exists::names_image;
use crate::images::split_reference;

const OS_LINUX: &str = "linux";
const ROOTFS_TYPE: &str = "layers";
const GRAPH_DRIVER: &str = "ward";
/// Docker's zero time, for an image that was never tagged.
const NEVER: &str = "0001-01-01T00:00:00Z";

const UNAME_ARM64: &str = "aarch64";
const UNAME_AMD64: &str = "x86_64";
const DOCKER_ARM64: &str = "arm64";
const DOCKER_AMD64: &str = "amd64";

/// Docker's name for a CPU architecture, from the one `uname` reports.
fn docker_arch(arch: &str) -> &str {
    match arch {
        UNAME_ARM64 => DOCKER_ARM64,
        UNAME_AMD64 => DOCKER_AMD64,
        other => other,
    }
}

fn created_text(image: &ImageInfo) -> String {
    image
        .created_at
        .as_ref()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.seconds, u32::try_from(t.nanos).ok()?))
        .map_or_else(
            || NEVER.to_string(),
            |t| t.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        )
}

/// `null` for an empty list, as Docker prints an unset slice.
fn list_or_null(items: &[String]) -> Value {
    if items.is_empty() {
        Value::Null
    } else {
        json!(items)
    }
}

/// The record `docker image inspect` prints. `tags` are every cached
/// reference of the image.
pub fn image_value(
    image: &ImageInfo,
    tags: &[String],
    config: &ImageHistoryResponse,
    daemon_arch: &str,
) -> Value {
    let mut digests: Vec<String> = tags
        .iter()
        .map(|tag| format!("{}@{}", split_reference(tag).0, image.digest))
        .collect();
    digests.dedup();
    json!({
        "Id": image.digest,
        "RepoTags": tags,
        "RepoDigests": digests,
        "Parent": "",
        "Comment": "",
        "Created": created_text(image),
        "DockerVersion": "",
        "Author": "",
        "Config": {
            "Hostname": "",
            "Domainname": "",
            "User": config.user,
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "Tty": false,
            "OpenStdin": false,
            "StdinOnce": false,
            "Env": list_or_null(&config.env),
            "Cmd": list_or_null(&config.cmd),
            "Image": "",
            "Volumes": null,
            "WorkingDir": config.working_dir,
            "Entrypoint": list_or_null(&config.entrypoint),
            "OnBuild": null,
            "Labels": null,
        },
        "Architecture": docker_arch(daemon_arch),
        "Os": OS_LINUX,
        "Size": image.size_bytes,
        "GraphDriver": {"Data": null, "Name": GRAPH_DRIVER},
        "RootFS": {"Type": ROOTFS_TYPE, "Layers": [image.digest]},
        "Metadata": {"LastTagTime": NEVER},
    })
}

/// The inspect record of the cached image `reference` names: a tag, or an ID
/// or its prefix.
///
/// # Errors
///
/// Fails with a not found status when no cached image matches.
pub async fn lookup(c: &mut WardClient<Channel>, reference: &str) -> Result<Value> {
    let images = c.list_images(()).await?.into_inner().images;
    let image = images
        .iter()
        .find(|image| names_image(image, reference))
        .ok_or_else(|| {
            anyhow!(tonic::Status::not_found(format!(
                "No such image: {reference}"
            )))
        })?;
    let tags: Vec<String> = images
        .iter()
        .filter(|other| other.digest == image.digest)
        .map(|other| other.reference.clone())
        .collect();
    let config = c
        .image_history(ImageHistoryRequest {
            reference: image.reference.clone(),
        })
        .await?
        .into_inner();
    let arch = c.get_info(()).await?.into_inner().arch;
    Ok(image_value(image, &tags, &config, &arch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached() -> ImageInfo {
        ImageInfo {
            reference: "docker.io/library/alpine:3".into(),
            digest: "sha256:abc".into(),
            size_bytes: 7_800_000,
            created_at: Some(prost_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
        }
    }

    #[test]
    fn given_an_image_when_valued_then_ids_tags_digests_and_size_follow_docker() {
        let tags = vec!["docker.io/library/alpine:3".to_string()];

        let value = image_value(
            &cached(),
            &tags,
            &ImageHistoryResponse::default(),
            "aarch64",
        );

        assert_eq!(value["Id"], "sha256:abc");
        assert_eq!(value["RepoTags"], json!(["docker.io/library/alpine:3"]));
        assert_eq!(
            value["RepoDigests"],
            json!(["docker.io/library/alpine@sha256:abc"])
        );
        assert_eq!(value["Created"], "2023-11-14T22:13:20Z");
        assert_eq!(value["Architecture"], "arm64");
        assert_eq!(value["Os"], "linux");
        assert_eq!(value["Size"], 7_800_000);
        assert_eq!(value["RootFS"]["Type"], "layers");
    }

    #[test]
    fn given_two_tags_of_one_repository_when_valued_then_the_repo_digest_is_listed_once() {
        let tags = vec![
            "docker.io/library/alpine:3".to_string(),
            "docker.io/library/alpine:latest".to_string(),
        ];

        let value = image_value(&cached(), &tags, &ImageHistoryResponse::default(), "x86_64");

        assert_eq!(value["RepoTags"].as_array().unwrap().len(), 2);
        assert_eq!(value["RepoDigests"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn given_a_stored_config_when_valued_then_run_settings_appear_and_empty_lists_are_null() {
        let config = ImageHistoryResponse {
            cmd: vec!["sh".into()],
            env: vec!["PATH=/bin".into()],
            working_dir: "/app".into(),
            user: "nobody".into(),
            ..Default::default()
        };

        let value = image_value(&cached(), &[], &config, "x86_64");

        assert_eq!(value["Config"]["Cmd"], json!(["sh"]));
        assert_eq!(value["Config"]["Env"], json!(["PATH=/bin"]));
        assert_eq!(value["Config"]["WorkingDir"], "/app");
        assert_eq!(value["Config"]["User"], "nobody");
        assert_eq!(value["Config"]["Entrypoint"], Value::Null);
        assert_eq!(value["Architecture"], "amd64");
    }
}
