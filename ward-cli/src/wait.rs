// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward wait SANDBOX...`: block until each sandbox's main process ends and
//! print its exit code, one per line like `docker wait`.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use ward_core::pb::WaitRequest;

use crate::client;
use crate::lifecycle::Failures;

/// Waits for each reference in turn. A reference that fails goes to stderr
/// and the rest are still waited for.
pub async fn run(socket: &str, token_store: &Path, refs: &[String], json: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let mut failures = Failures::new();
    for reference in refs {
        let result = async {
            let target = client::resolve(&mut c, token_store, reference).await?;
            let request = target.request(WaitRequest {
                sandbox_id: target.id.clone(),
            })?;
            Ok::<_, anyhow::Error>(c.wait(request).await?.into_inner().exit_code)
        }
        .await;
        match result {
            Ok(code) if json => println!("{}", json!({"sandbox": reference, "exit_code": code})),
            Ok(code) => println!("{code}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(refs.len(), "waited for")
}
