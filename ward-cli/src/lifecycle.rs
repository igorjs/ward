// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward stop / start / restart / rm / prune`.
//!
//! Each command takes sandbox references (name, id or id prefix), works
//! through them in order and keeps going after a failure, like Docker: a
//! success prints the reference, a failure goes to stderr, and the command
//! exits nonzero if any reference failed.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    RemoveSandboxRequest, RestartRequest, SandboxStatus, StartRequest, StopRequest,
};

use crate::client::{self, Resolved};
use crate::tokens;

/// The state change a lifecycle command asks for.
#[derive(Clone, Copy)]
pub enum Action {
    Stop,
    Start,
    Restart,
}

impl Action {
    /// Past tense, used as the JSON key and in the failure summary.
    fn past(self) -> &'static str {
        match self {
            Self::Stop => "stopped",
            Self::Start => "started",
            Self::Restart => "restarted",
        }
    }

    async fn call(self, c: &mut WardClient<Channel>, target: &Resolved) -> Result<()> {
        let id = target.id.clone();
        match self {
            Self::Stop => {
                let request = StopRequest { id };
                c.stop(target.request(request)?).await?;
            }
            Self::Start => {
                c.start(target.request(StartRequest { id })?).await?;
            }
            Self::Restart => {
                c.restart(target.request(RestartRequest { id })?).await?;
            }
        }
        Ok(())
    }
}

/// Tallies per-reference failures and prints each one to stderr.
pub struct Failures {
    count: usize,
}

impl Failures {
    pub fn new() -> Self {
        Self { count: 0 }
    }

    pub fn record(&mut self, subject: &str, error: &anyhow::Error) {
        self.count += 1;
        eprintln!("error: {subject}: {}", error_text(error));
    }

    pub fn finish(self, total: usize, did_what: &str) -> Result<()> {
        if self.count > 0 {
            anyhow::bail!(
                "{} of {total} sandboxes could not be {did_what}",
                self.count
            );
        }
        Ok(())
    }
}

/// A daemon status shows its message only, without the transport framing.
fn error_text(error: &anyhow::Error) -> String {
    match error.downcast_ref::<tonic::Status>() {
        Some(status) => status.message().to_string(),
        None => format!("{error:#}"),
    }
}

pub async fn lifecycle(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    action: Action,
    refs: &[String],
    json: bool,
) -> Result<()> {
    let mut failures = Failures::new();
    for reference in refs {
        let result = async {
            let target = client::resolve(c, token_store, reference).await?;
            action.call(c, &target).await
        }
        .await;
        match result {
            Ok(()) if json => println!("{}", json!({ action.past(): reference })),
            Ok(()) => println!("{reference}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(refs.len(), action.past())
}

pub async fn remove(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    refs: &[String],
    force: bool,
    json: bool,
) -> Result<()> {
    let mut failures = Failures::new();
    for reference in refs {
        let result = async {
            let target = client::resolve(c, token_store, reference).await?;
            // Only the CLI refuses; the RPC keeps removing running sandboxes
            // so SDKs and ward-mcp behave as before.
            if !force && target.status == SandboxStatus::Running {
                anyhow::bail!("cannot remove a running sandbox: stop it first or use -f");
            }
            remove_resolved(c, token_store, &target).await
        }
        .await;
        match result {
            Ok(()) if json => println!("{}", json!({"removed": reference})),
            Ok(()) => println!("{reference}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(refs.len(), "removed")
}

/// Remove every stopped and exited sandbox, skipping those the CLI holds no
/// token for.
pub async fn prune(c: &mut WardClient<Channel>, token_store: &Path, json: bool) -> Result<()> {
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    let mut failures = Failures::new();
    let mut candidates = 0;

    for info in &listed {
        let target = Resolved::from_info(token_store, info)?;
        if !matches!(
            target.status,
            SandboxStatus::Stopped | SandboxStatus::Exited
        ) {
            continue;
        }
        candidates += 1;
        if !target.has_token() {
            skipped.push(target.id);
            continue;
        }
        match remove_resolved(c, token_store, &target).await {
            Ok(()) => removed.push(target.id),
            Err(e) => failures.record(&target.id, &e),
        }
    }

    if json {
        println!("{}", json!({"removed": removed, "skipped": skipped}));
    } else {
        for id in &removed {
            println!("{id}");
        }
        for id in &skipped {
            eprintln!("skipped: {id}: no token in the local token store");
        }
    }
    failures.finish(candidates, "removed")
}

/// Remove a resolved sandbox and drop its token from the store.
pub async fn remove_resolved(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    target: &Resolved,
) -> Result<()> {
    let request = RemoveSandboxRequest {
        id: target.id.clone(),
    };
    c.remove_sandbox(target.request(request)?).await?;
    // The daemon already removed the sandbox, so a store failure is only a
    // warning: the stale entry is harmless, the daemon rejects its token.
    if let Err(e) = tokens::remove(token_store, &target.id) {
        eprintln!("warning: could not prune local token store: {e}");
    }
    Ok(())
}
