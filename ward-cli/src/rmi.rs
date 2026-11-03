// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward rmi [-f] IMAGE...`: removes tags and images like `docker rmi`.

use anyhow::Result;
use ward_core::pb::RemoveImageRequest;

use crate::client;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Delete an image that stopped sandboxes use or that has several tags.
    /// An image a running sandbox uses is never deleted.
    #[arg(short, long)]
    pub force: bool,
    /// Tag, image ID or unique ID prefix.
    #[arg(required = true, value_name = "IMAGE")]
    pub images: Vec<String>,
}

/// Removes each image in turn. A failure is printed and the rest still run;
/// the command fails if any did.
pub async fn run(socket: &str, args: Args) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let mut failed = Vec::new();
    for image in args.images {
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
