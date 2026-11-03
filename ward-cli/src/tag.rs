// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward tag SOURCE TARGET`: names a cached image a second time.

use anyhow::Result;
use ward_core::pb::TagImageRequest;

use crate::client;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Tag, image ID or unique ID prefix.
    pub source: String,
    /// New tag, REPOSITORY[:TAG].
    pub target: String,
}

pub async fn run(socket: &str, args: Args) -> Result<()> {
    let mut c = client::connect(socket).await?;
    c.tag_image(TagImageRequest {
        source: args.source,
        target: args.target,
    })
    .await?;
    Ok(())
}
