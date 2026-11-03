// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward pause SANDBOX...`: freezes the processes of running sandboxes.
//!
//! Like the other lifecycle commands it works through the references in
//! order, prints each one on success and reports each failure on stderr.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use ward_core::pb::PauseRequest;

use crate::client;
use crate::lifecycle::Failures;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Sandbox names, IDs or ID prefixes.
    #[arg(required = true)]
    pub refs: Vec<String>,
}

pub async fn run(socket: &str, token_store: &Path, args: Args, json: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let mut failures = Failures::new();
    for reference in &args.refs {
        let result = async {
            let target = client::resolve(&mut c, token_store, reference).await?;
            let request = PauseRequest {
                id: target.id.clone(),
            };
            c.pause(target.request(request)?).await?;
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) if json => println!("{}", json!({"paused": reference})),
            Ok(()) => println!("{reference}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(args.refs.len(), "paused")
}
