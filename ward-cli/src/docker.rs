// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward run`, `ward exec -it` and `ward logs`: Docker-style commands that
//! stream a process through Attach and exit with its exit code.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use ward_core::pb::{AttachMode, AttachStart, CreateSandboxRequest};

use crate::attach::{self, require_exit, require_terminal};
use crate::client::{self, Resolved};
use crate::{lifecycle, tokens};

/// The `ward run` flags that change how the process is attached.
pub struct RunFlags {
    pub interactive: bool,
    pub tty: bool,
    pub rm: bool,
    pub detach: bool,
}

/// Creates a sandbox, runs its main command and returns the exit code.
pub async fn run(
    socket: &str,
    token_store: &Path,
    request: CreateSandboxRequest,
    flags: RunFlags,
) -> Result<i32> {
    if request.command.is_empty() && !flags.detach {
        bail!("a command is required unless the sandbox runs detached (-d)");
    }
    require_terminal(flags.tty)?;
    let mut c = client::connect(socket).await?;
    let created = c.create_sandbox(request).await?.into_inner();
    if let Err(e) = tokens::save(token_store, &created.id, &created.token) {
        eprintln!("warning: could not persist token locally: {e}");
    }
    if flags.detach {
        println!("{}", created.id);
        return Ok(0);
    }

    let target = Resolved::with_token(&created, created.token.clone());
    let start = AttachStart {
        sandbox_id: created.id.clone(),
        mode: AttachMode::MainProcess as i32,
        tty: flags.tty,
        ..Default::default()
    };
    let outcome = attach::attach(&mut c, &target, start, flags.interactive).await;
    if flags.rm
        && let Err(e) = lifecycle::remove_resolved(&mut c, token_store, &target).await
    {
        eprintln!("warning: could not remove the sandbox: {e}");
    }
    require_exit(outcome?)
}

/// What `ward exec -i/-t` runs inside an existing sandbox.
pub struct ExecSpec {
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub workdir: String,
    pub interactive: bool,
    pub tty: bool,
}

/// Runs a command in a sandbox and returns its exit code.
pub async fn exec(
    socket: &str,
    token_store: &Path,
    reference: &str,
    spec: ExecSpec,
) -> Result<i32> {
    require_terminal(spec.tty)?;
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let start = AttachStart {
        sandbox_id: target.id.clone(),
        mode: AttachMode::Exec as i32,
        command: spec.command,
        tty: spec.tty,
        env: spec.env,
        workdir: spec.workdir,
        ..Default::default()
    };
    require_exit(attach::attach(&mut c, &target, start, spec.interactive).await?)
}

/// Prints the buffered output of a sandbox's main process, then keeps
/// following it when `follow` is set. Returns the code to exit with: 0, or
/// `128 + signo` when a signal ended the follow. The sandbox keeps running.
pub async fn logs(socket: &str, token_store: &Path, reference: &str, follow: bool) -> Result<i32> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let start = AttachStart {
        sandbox_id: target.id.clone(),
        mode: AttachMode::MainProcess as i32,
        replay: true,
        follow,
        ..Default::default()
    };
    // No exit status means the output simply ended.
    Ok(attach::attach(&mut c, &target, start, false)
        .await?
        .unwrap_or(0))
}
