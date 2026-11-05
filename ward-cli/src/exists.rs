// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward container exists` and `ward image exists`, which Podman has: no
//! output, and the exit code says the answer.

use ward_core::pb::{GetSandboxRequest, ImageInfo};

use crate::client;

/// Exit code when the object exists.
pub const EXISTS: i32 = 0;
/// Exit code when it does not.
pub const MISSING: i32 = 1;
/// Exit code when the check itself failed, as Podman uses.
pub const FAILED: i32 = 125;
const DEFAULT_TAG: &str = "latest";
const DIGEST_PREFIX: &str = "sha256:";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Name, ID or ID prefix.
    pub name: String,
}

/// The exit code for a sandbox named `reference`.
pub async fn container(socket: &str, reference: &str) -> i32 {
    let answer = async {
        let mut c = client::connect(socket).await?;
        match c
            .get_sandbox(GetSandboxRequest {
                id: reference.to_string(),
            })
            .await
        {
            Ok(_) => Ok(EXISTS),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(MISSING),
            Err(status) => Err(anyhow::Error::from(status)),
        }
    }
    .await;
    code_of(answer)
}

/// The exit code for an image named `reference`: a tag, or an ID or its prefix.
pub async fn image(socket: &str, reference: &str) -> i32 {
    let answer = async {
        let mut c = client::connect(socket).await?;
        let images = c.list_images(()).await?.into_inner().images;
        anyhow::Ok(if images.iter().any(|i| names_image(i, reference)) {
            EXISTS
        } else {
            MISSING
        })
    }
    .await;
    code_of(answer)
}

/// The exit code for a volume named `reference`: a name, an ID or a unique
/// ID prefix.
pub async fn volume(socket: &str, reference: &str) -> i32 {
    let answer = async {
        let mut c = client::connect(socket).await?;
        let volumes = c.list_volumes(()).await?.into_inner().volumes;
        anyhow::Ok(if crate::volume::find(&volumes, reference).is_ok() {
            EXISTS
        } else {
            MISSING
        })
    }
    .await;
    code_of(answer)
}

fn code_of(answer: anyhow::Result<i32>) -> i32 {
    answer.unwrap_or_else(|e| {
        eprintln!("Error: {e:?}");
        FAILED
    })
}

/// Whether `reference` is one of the image's tags, with `:latest` implied, or
/// a prefix of its ID.
pub(crate) fn names_image(image: &ImageInfo, reference: &str) -> bool {
    let wanted = reference.trim_start_matches(DIGEST_PREFIX);
    if wanted.is_empty() {
        return false;
    }
    let tagged = match reference.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => reference.to_string(),
        _ => format!("{reference}:{DEFAULT_TAG}"),
    };
    let id = image.digest.trim_start_matches(DIGEST_PREFIX);
    image.reference == tagged || image.reference == reference || id.starts_with(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(reference: &str, digest: &str) -> ImageInfo {
        ImageInfo {
            reference: reference.into(),
            digest: digest.into(),
            ..Default::default()
        }
    }

    #[rstest::rstest]
    #[case::exact_tag("alpine:3.20", true)]
    #[case::default_tag("alpine", false)]
    #[case::other_tag("alpine:3.19", false)]
    #[case::id_prefix("abc123", true)]
    #[case::full_digest("sha256:abc123def", true)]
    #[case::unrelated_prefix("fff", false)]
    #[case::empty("", false)]
    #[case::bare_digest_prefix("sha256:", false)]
    fn given_an_image_when_named_then_tag_and_id_prefix_match(
        #[case] reference: &str,
        #[case] expected: bool,
    ) {
        let cached = image("alpine:3.20", "sha256:abc123def");

        assert_eq!(names_image(&cached, reference), expected);
    }

    #[test]
    fn given_a_name_without_a_tag_when_the_image_is_latest_then_it_matches() {
        let cached = image("alpine:latest", "sha256:abc");

        assert!(names_image(&cached, "alpine"));
    }

    #[test]
    fn given_a_failed_check_when_coded_then_it_is_125() {
        assert_eq!(code_of(Err(anyhow::anyhow!("daemon down"))), FAILED);
        assert_eq!(code_of(Ok(MISSING)), MISSING);
    }
}
