// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward load [-i FILE]`: reads a Docker or OCI layout tar into the cache.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

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
    let source = open_input(args.input.as_deref()).await?;
    let mut c = client::connect(socket).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(UPLOAD_BUFFER);
    let reader = tokio::spawn(read_chunks(source, tx, |data| ArchiveChunk { data }));
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

pub type Source = Box<dyn AsyncRead + Unpin + Send>;

/// The file at `path`, or standard input when there is none. Docker refuses
/// to read an archive from a terminal.
pub async fn open_input(path: Option<&Path>) -> Result<Source> {
    match path {
        Some(path) => Ok(Box::new(tokio::fs::File::open(path).await.with_context(
            || format!("open {}: no such file or directory", path.display()),
        )?)),
        None if std::io::stdin().is_terminal() => Err(anyhow::Error::new(client::PlainError(
            "requested load from stdin, but stdin is empty".to_string(),
        ))),
        None => Ok(Box::new(tokio::io::stdin())),
    }
}

/// Sends `source` to `tx` in chunks, each wrapped into a message by `wrap`.
pub async fn read_chunks<T>(
    mut source: Source,
    tx: tokio::sync::mpsc::Sender<T>,
    wrap: impl Fn(Vec<u8>) -> T,
) -> Result<()> {
    loop {
        let mut data = vec![0; CHUNK_BYTES];
        let read = source.read(&mut data).await?;
        if read == 0 {
            return Ok(());
        }
        data.truncate(read);
        if tx.send(wrap(data)).await.is_err() {
            return Ok(());
        }
    }
}
