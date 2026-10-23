// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward pull IMAGE`: prints the daemon's progress lines like `docker pull`.

use anyhow::Result;
use ward_core::pb::PullImageRequest;

use crate::client;

/// Pulls `reference`. With `quiet`, only the image reference is printed once
/// the pull is done.
pub async fn run(socket: &str, reference: &str, quiet: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let request = PullImageRequest {
        reference: reference.to_string(),
    };
    let mut stream = c.pull_image(request).await?.into_inner();
    while let Some(line) = stream.message().await? {
        if !quiet {
            println!("{}", line_text(&line.id, &line.status));
        }
    }
    if quiet {
        println!("{reference}");
    }
    Ok(())
}

/// `id: status` for a line about a layer or tag, the bare status otherwise.
fn line_text(id: &str, status: &str) -> String {
    if id.is_empty() {
        status.to_string()
    } else {
        format!("{id}: {status}")
    }
}
