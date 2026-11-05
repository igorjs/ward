// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward rmi [-f] IMAGE...`: removes tags and images like `docker rmi`.

use anyhow::Result;
use ward_core::pb::{ImageInfo, RemoveImageRequest};

use crate::client;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Delete an image that stopped sandboxes use or that has several tags.
    /// An image a running sandbox uses is never deleted.
    #[arg(short, long)]
    pub force: bool,
    /// Remove every image in the cache (Podman). Give no IMAGE with it. An
    /// empty cache prints nothing and succeeds; an image a running sandbox
    /// uses is reported as an error, the rest are still removed.
    #[arg(short, long, conflicts_with = "images")]
    pub all: bool,
    /// Tag, image ID or unique ID prefix.
    #[arg(required_unless_present = "all", value_name = "IMAGE")]
    pub images: Vec<String>,
}

/// What names a cached image: its tag, else its digest.
fn image_name(image: &ImageInfo) -> String {
    if image.reference.is_empty() {
        image.digest.clone()
    } else {
        image.reference.clone()
    }
}

/// The names with repeats dropped, in first-seen order, so an image listed
/// twice is not removed twice.
fn distinct(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names.filter(|name| seen.insert(name.clone())).collect()
}

/// Removes each image in turn. A failure is printed and the rest still run;
/// the command fails if any did.
pub async fn run(socket: &str, args: Args) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let images = if args.all {
        let cached = c.list_images(()).await?.into_inner().images;
        distinct(cached.iter().map(image_name))
    } else {
        args.images
    };
    let mut failed = Vec::new();
    for image in images {
        let request = RemoveImageRequest {
            reference: image.clone(),
            force: args.force,
        };
        match c.remove_image(request).await {
            Ok(reply) => {
                let reply = reply.into_inner();
                for reference in reply.untagged {
                    println!("Untagged: {reference}");
                }
                for id in reply.deleted {
                    println!("Deleted: {id}");
                }
            }
            Err(status) => {
                let error = anyhow::Error::from(status);
                match client::daemon_error_line(&error) {
                    Some(line) => eprintln!("{line}"),
                    None => eprintln!("Error: {error:?}"),
                }
                failed.push(image);
            }
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(anyhow::Error::new(client::PlainError(format!(
        "Error: failed to remove images: [{}]",
        failed.join(" ")
    ))))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: Args,
    }

    fn image(reference: &str, digest: &str) -> ImageInfo {
        ImageInfo {
            reference: reference.to_string(),
            digest: digest.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn given_a_tagged_image_when_named_then_the_tag_is_used() {
        assert_eq!(image_name(&image("alpine:3", "sha256:ab")), "alpine:3");
    }

    #[test]
    fn given_an_untagged_image_when_named_then_the_digest_is_used() {
        assert_eq!(image_name(&image("", "sha256:ab")), "sha256:ab");
    }

    #[test]
    fn given_repeated_names_when_made_distinct_then_first_seen_order_is_kept() {
        let names = ["b", "a", "b", "c", "a"].map(String::from);

        assert_eq!(distinct(names.into_iter()), ["b", "a", "c"]);
    }

    #[test]
    fn given_all_without_images_when_parsed_then_it_is_accepted() {
        assert!(Wrapper::try_parse_from(["rmi", "-a"]).is_ok());
    }

    #[test]
    fn given_all_with_an_image_when_parsed_then_it_conflicts() {
        assert!(Wrapper::try_parse_from(["rmi", "-a", "alpine"]).is_err());
    }

    #[test]
    fn given_neither_all_nor_an_image_when_parsed_then_it_fails() {
        assert!(Wrapper::try_parse_from(["rmi"]).is_err());
    }
}
