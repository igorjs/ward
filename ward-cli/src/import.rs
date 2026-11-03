// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward import FILE|- [REPOSITORY[:TAG]]`: a tar becomes a one-layer image.

use std::path::Path;

use anyhow::{Result, bail};
use ward_core::pb::ImportImageMessage;

use crate::{client, load};

const STDIN_NAME: &str = "-";
const UPLOAD_BUFFER: usize = 4;
const URL_SCHEMES: [&str; 2] = ["http://", "https://"];

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Apply a Dockerfile instruction to the image config: CMD, ENTRYPOINT,
    /// ENV, WORKDIR or USER. Repeatable.
    #[arg(short = 'c', long = "change", value_name = "CHANGE")]
    pub change: Vec<String>,
    /// Message for the image history.
    #[arg(short, long)]
    pub message: Option<String>,
    /// Tar file, or `-` for standard input. The tar may be gzip compressed.
    pub file: String,
    /// Local tag for the new image, REPOSITORY[:TAG].
    pub repository: Option<String>,
}

pub async fn run(socket: &str, args: Args) -> Result<()> {
    if URL_SCHEMES
        .iter()
        .any(|scheme| args.file.starts_with(scheme))
    {
        bail!("importing from a URL is not supported; download the file first");
    }
    let path = (args.file != STDIN_NAME).then(|| Path::new(&args.file));
    let source = load::open_input(path).await?;
    let mut c = client::connect(socket).await?;
    let (tx, rx) = tokio::sync::mpsc::channel(UPLOAD_BUFFER);
    tx.send(ImportImageMessage {
        reference: args.repository.unwrap_or_default(),
        message: args.message.unwrap_or_default(),
        changes: args.change,
        data: Vec::new(),
    })
    .await?;
    let reader = tokio::spawn(load::read_chunks(source, tx, |data| ImportImageMessage {
        data,
        ..ImportImageMessage::default()
    }));
    let reply = c
        .import_image(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await;
    reader.await??;
    println!("{}", reply?.into_inner().image_id);
    Ok(())
}
