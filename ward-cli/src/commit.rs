// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward commit SANDBOX [REPOSITORY[:TAG]]`: prints the new image id like
//! `docker commit`.

use std::path::Path;

use anyhow::Result;
use ward_core::pb::CommitRequest;

use crate::client;

/// The flags of `ward commit`, as the daemon takes them.
pub struct Options {
    pub author: String,
    pub message: String,
    pub changes: Vec<String>,
    pub pause: bool,
    /// Empty for an untagged image.
    pub reference: String,
}

/// Commits the sandbox `container` and prints the image id.
pub async fn run(
    socket: &str,
    token_store: &Path,
    container: &str,
    options: Options,
) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, container).await?;
    let request = target.request(CommitRequest {
        sandbox_id: target.id.clone(),
        reference: options.reference,
        author: options.author,
        message: options.message,
        changes: options.changes,
        pause: options.pause,
    })?;
    let reply = c.commit(request).await?.into_inner();
    println!("{}", reply.image_id);
    Ok(())
}
