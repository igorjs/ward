// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward start -a` and `-i`: starts a sandbox, then attaches to its main
//! process and exits with the process's exit code, as `docker start` does.

use std::path::Path;

use anyhow::{Result, bail};
use ward_core::pb::StartRequest;

use crate::attach_cmd::{self, SessionFlags as AttachFlags};
use crate::client;

/// What Docker says about several references with `-a` or `-i`.
const ONE_ONLY: &str = "you cannot start and attach multiple containers at once";

#[derive(clap::Args, Debug)]
pub struct StartFlags {
    /// Print the process's output and exit with its exit code.
    #[arg(short = 'a', long)]
    pub attach: bool,
    /// Send this terminal's input to the process. Attaches too.
    #[arg(short = 'i', long)]
    pub interactive: bool,
    #[command(flatten)]
    pub attach_flags: AttachFlags,
}

impl StartFlags {
    /// True when the sandbox is to be attached after it starts.
    pub fn wanted(&self) -> bool {
        self.attach || self.interactive
    }
}

/// Starts the sandbox, attaches and returns the code to exit with.
pub async fn run(
    socket: &str,
    token_store: &Path,
    refs: &[String],
    flags: &StartFlags,
) -> Result<i32> {
    let [reference] = refs else {
        bail!(ONE_ONLY);
    };
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let started = c
        .start(target.request(StartRequest {
            id: target.id.clone(),
        })?)
        .await?
        .into_inner();
    let target = client::Resolved::from_info(token_store, &started)?;
    attach_cmd::attach_main(
        &mut c,
        &target,
        flags.interactive,
        &flags.attach_flags,
        true,
    )
    .await
}
