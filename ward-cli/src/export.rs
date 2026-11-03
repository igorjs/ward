// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward export [-o FILE] SANDBOX`: the sandbox's filesystem as a tar.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ward_core::pb::ExportSandboxRequest;

use crate::{client, save};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Write to FILE instead of standard output.
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<PathBuf>,
    /// Sandbox name, ID or ID prefix.
    pub container: String,
}

pub async fn run(socket: &str, token_store: &Path, args: Args) -> Result<()> {
    save::refuse_terminal(args.output.as_deref())?;
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, &args.container).await?;
    let request = target.request(ExportSandboxRequest {
        sandbox_id: target.id.clone(),
    })?;
    let mut stream = c.export_sandbox(request).await?.into_inner();
    save::write_stream(&mut stream, args.output.as_deref()).await
}
