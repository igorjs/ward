// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward save [-o FILE] IMAGE...`: writes images as an OCI layout tar.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use ward_core::pb::SaveImagesRequest;

use crate::client;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Write to FILE instead of standard output.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Tag, image ID or unique ID prefix.
    #[arg(required = true, value_name = "IMAGE")]
    pub images: Vec<String>,
}

pub async fn run(socket: &str, args: Args) -> Result<()> {
    if args.output.is_none() && std::io::stdout().is_terminal() {
        return Err(anyhow::Error::new(client::PlainError(
            "cowardly refusing to save to a terminal. Use the -o flag or redirect".to_string(),
        )));
    }
    let mut c = client::connect(socket).await?;
    let mut stream = c
        .save_images(SaveImagesRequest {
            references: args.images,
        })
        .await?
        .into_inner();
    match &args.output {
        Some(path) => {
            let file = tokio::fs::File::create(path)
                .await
                .with_context(|| format!("cannot create {}", path.display()))?;
            let copied = copy_chunks(&mut stream, file).await;
            if copied.is_err() {
                let _ = tokio::fs::remove_file(path).await;
            }
            copied
        }
        None => copy_chunks(&mut stream, tokio::io::stdout()).await,
    }
}

async fn copy_chunks(
    stream: &mut tonic::Streaming<ward_core::pb::ArchiveChunk>,
    mut out: impl AsyncWrite + Unpin,
) -> Result<()> {
    while let Some(chunk) = stream.message().await? {
        out.write_all(&chunk.data).await?;
    }
    out.flush().await?;
    Ok(())
}
