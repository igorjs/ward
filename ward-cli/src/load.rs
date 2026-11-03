// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward load [-i FILE]`: reads a Docker or OCI layout tar into the cache.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use ward_core::pb::ArchiveChunk;

use crate::client;

/// Bytes of the archive sent in one message.
const CHUNK_BYTES: usize = 256 * 1024;
const UPLOAD_BUFFER: usize = 4;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Read from FILE instead of standard input.
    #[arg(short, long, value_name = "FILE")]
    pub input: Option<PathBuf>,
    /// Print nothing.
    #[arg(short, long)]
    pub quiet: bool,
}

pub async fn run(socket: &str, args: Args) -> Result<()> {
    let source: Box<dyn AsyncRead + Unpin + Send> = match &args.input {
        Some(path) => Box::new(
            tokio::fs::File::open(path)
                .await
                .with_context(|| format!("open {}: no such file or directory", path.display()))?,
        ),
        None if std::io::stdin().is_terminal() => {
            return Err(anyhow::Error::new(client::PlainError(
                "requested load from stdin, but stdin is empty".to_string(),
            )));
        }
        None => Box::new(tokio::io::stdin()),
    };
    let mut c = client::connect(socket).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(UPLOAD_BUFFER);
    let reader = tokio::spawn(read_chunks(source, tx));
    let reply = c
        .load_images(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await;
    reader.await??;
    let reply = reply?.into_inner();
    if !args.quiet {
        for image in reply.images {
            if image.references.is_empty() {
                println!("Loaded image ID: {}", image.image_id);
            }
            for reference in image.references {
                println!("Loaded image: {reference}");
            }
        }
    }
    Ok(())
}

async fn read_chunks(
    mut source: Box<dyn AsyncRead + Unpin + Send>,
    tx: tokio::sync::mpsc::Sender<ArchiveChunk>,
) -> Result<()> {
    loop {
        let mut data = vec![0; CHUNK_BYTES];
        let read = source.read(&mut data).await?;
        if read == 0 {
            return Ok(());
        }
        data.truncate(read);
        if tx.send(ArchiveChunk { data }).await.is_err() {
            return Ok(());
        }
    }
}
