// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward run`, `ward exec` and `ward logs`: Docker-style commands that
//! stream a process through Attach and exit with its exit code.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use ward_core::pb::{AttachMode, AttachStart, CreateSandboxRequest, SecretFile};

use crate::attach::{self, require_exit, require_terminal};
use crate::client::{self, Resolved};
use crate::log_output::{LogOptions, LogPrinter, Printable};
use crate::terminal::ExitSignals;
use crate::{lifecycle, tokens};

pub const TAIL_ALL: &str = "all";
/// How long `--rm` waits for the daemon to remove the sandbox.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);

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
    require_terminal(flags.tty, flags.interactive)?;
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
    // Registered before the attach so a Ctrl-C between its end and the
    // removal is not lost.
    let mut exits = if flags.rm {
        ExitSignals::register(true).ok()
    } else {
        None
    };
    let outcome = match exits.as_mut() {
        Some(exits) => {
            attach::attach_registered(&mut c, &target, start, flags.interactive, exits).await
        }
        None => attach::attach(&mut c, &target, start, flags.interactive).await,
    };
    if flags.rm {
        let removal = lifecycle::remove_resolved(&mut c, token_store, &target);
        let interrupt = async {
            match exits.as_mut() {
                Some(exits) => exits.recv_exit_code().await,
                None => std::future::pending().await,
            }
        };
        let interrupted = remove_unless_interrupted(removal, interrupt, REMOVE_TIMEOUT).await;
        if let Some(code) = interrupted {
            return Ok(code);
        }
    }
    require_exit(outcome?)
}

/// Runs the removal, warning on failure or after `limit`. Returns
/// the exit code of a signal that arrived first.
async fn remove_unless_interrupted(
    removal: impl Future<Output = Result<()>>,
    interrupt: impl Future<Output = i32>,
    limit: Duration,
) -> Option<i32> {
    tokio::select! {
        code = interrupt => {
            eprintln!("warning: interrupted while removing the sandbox");
            Some(code)
        }
        done = tokio::time::timeout(limit, removal) => {
            match done {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("warning: could not remove the sandbox: {e}"),
                Err(_) => eprintln!("warning: timed out removing the sandbox"),
            }
            None
        }
    }
}

/// What `ward exec` runs inside an existing sandbox.
pub struct ExecSpec {
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub workdir: String,
    pub user: String,
    pub interactive: bool,
    pub tty: bool,
    pub secrets: Vec<SecretFile>,
}

/// Runs a command in a sandbox and returns its exit code.
pub async fn exec(
    socket: &str,
    token_store: &Path,
    reference: &str,
    spec: ExecSpec,
) -> Result<i32> {
    require_terminal(spec.tty, spec.interactive)?;
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let start = AttachStart {
        sandbox_id: target.id.clone(),
        mode: AttachMode::Exec as i32,
        command: spec.command,
        tty: spec.tty,
        env: spec.env,
        workdir: spec.workdir,
        user: spec.user,
        secrets: spec.secrets,
        ..Default::default()
    };
    require_exit(attach::attach(&mut c, &target, start, spec.interactive).await?)
}

/// Prints the buffered output of a sandbox's main process, then keeps
/// following it when `follow` is set. Returns the code to exit with: 0, or
/// `128 + signo` when a signal ended the follow. The sandbox keeps running.
pub async fn logs(
    socket: &str,
    token_store: &Path,
    reference: &str,
    follow: bool,
    options: LogOptions,
) -> Result<i32> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let start = AttachStart {
        sandbox_id: target.id.clone(),
        mode: AttachMode::MainProcess as i32,
        replay: true,
        follow,
        ..Default::default()
    };
    let mut printer = LogPrinter::new(options);
    // No exit status means the output simply ended.
    let code = attach::attach_with(&mut c, &target, start, false, |output| {
        print_all(printer.accept(output))
    })
    .await?
    .unwrap_or(0);
    print_all(printer.finish())?;
    Ok(code)
}

fn print_all(printables: Vec<Printable>) -> std::io::Result<()> {
    printables
        .into_iter()
        .try_for_each(|(stream, bytes)| attach::write_to_terminal(stream, &bytes))
}

/// Parses `--tail`: `all` keeps every line, a number keeps that many.
pub fn parse_tail(value: &str) -> Result<Option<usize>> {
    if value == TAIL_ALL {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| anyhow::anyhow!("invalid value '{value}' for --tail: use a number or 'all'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGINT_EXIT_CODE: i32 = 130;
    const SHORT_LIMIT: Duration = Duration::from_millis(20);

    #[tokio::test]
    async fn given_slow_removal_when_interrupted_then_it_returns_130() {
        // Arrange
        let removal = std::future::pending::<Result<()>>();
        let interrupt = async { SIGINT_EXIT_CODE };

        // Act
        let code = remove_unless_interrupted(removal, interrupt, REMOVE_TIMEOUT).await;

        // Assert
        assert_eq!(code, Some(SIGINT_EXIT_CODE));
    }

    #[tokio::test]
    async fn given_hung_removal_when_timeout_passes_then_it_gives_up_without_a_code() {
        // Arrange
        let removal = std::future::pending::<Result<()>>();

        // Act
        let code = remove_unless_interrupted(removal, std::future::pending(), SHORT_LIMIT).await;

        // Assert
        assert_eq!(code, None);
    }

    #[test]
    fn given_tail_values_when_parsed_then_all_is_none_and_numbers_are_counts() {
        assert_eq!(parse_tail("all").unwrap(), None);
        assert_eq!(parse_tail("3").unwrap(), Some(3));
        assert!(parse_tail("-1").is_err());
        assert!(parse_tail("few").is_err());
    }
}
