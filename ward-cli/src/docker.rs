// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward run`, `ward exec` and `ward logs`: Docker-style commands that
//! stream a process through Attach and exit with its exit code.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use ward_core::pb::{AttachMode, AttachStart, AttachStream, CreateSandboxRequest};

use crate::attach::{self, require_exit, require_terminal};
use crate::client::{self, Resolved};
use crate::{lifecycle, tokens};

pub const TAIL_ALL: &str = "all";

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
    let outcome = attach::attach(&mut c, &target, start, flags.interactive).await;
    if flags.rm
        && let Err(e) = lifecycle::remove_resolved(&mut c, token_store, &target).await
    {
        eprintln!("warning: could not remove the sandbox: {e}");
    }
    require_exit(outcome?)
}

/// What `ward exec` runs inside an existing sandbox.
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
    tail: Option<usize>,
) -> Result<i32> {
    if follow && tail.is_some() {
        bail!("--tail cannot be combined with --follow");
    }
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
    let Some(lines) = tail else {
        return Ok(attach::attach(&mut c, &target, start, false)
            .await?
            .unwrap_or(0));
    };
    let mut chunks = Vec::new();
    let code = attach::attach_with(&mut c, &target, start, false, |stream, bytes| {
        chunks.push((stream, bytes.to_vec()));
        Ok(())
    })
    .await?
    .unwrap_or(0);
    for (stream, bytes) in tail_chunks(&chunks, lines) {
        attach::write_to_terminal(stream, bytes)?;
    }
    Ok(code)
}

/// The parts of `chunks` that make up their last `lines` lines, each paired
/// with the stream it came from.
fn tail_chunks(chunks: &[(AttachStream, Vec<u8>)], lines: usize) -> Vec<(AttachStream, &[u8])> {
    let all: Vec<u8> = chunks
        .iter()
        .flat_map(|(_, bytes)| bytes)
        .copied()
        .collect();
    let mut skip = all.len() - last_lines(&all, lines).len();
    let mut kept = Vec::new();
    for (stream, bytes) in chunks {
        let dropped = skip.min(bytes.len());
        skip -= dropped;
        if dropped < bytes.len() {
            kept.push((*stream, &bytes[dropped..]));
        }
    }
    kept
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

/// The last `count` lines of `output`, keeping their line endings.
fn last_lines(output: &[u8], count: usize) -> &[u8] {
    let body = output.strip_suffix(b"\n").unwrap_or(output);
    let mut start = body.len();
    for _ in 0..count {
        match body[..start].iter().rposition(|&b| b == b'\n') {
            Some(newline) => start = newline,
            None => return output,
        }
    }
    if count == 0 {
        return &[];
    }
    &output[start + 1..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_tail_values_when_parsed_then_all_is_none_and_numbers_are_counts() {
        assert_eq!(parse_tail("all").unwrap(), None);
        assert_eq!(parse_tail("3").unwrap(), Some(3));
        assert!(parse_tail("-1").is_err());
        assert!(parse_tail("few").is_err());
    }

    #[test]
    fn given_mixed_streams_when_tailed_then_each_kept_part_keeps_its_stream() {
        // Arrange
        let chunks = vec![
            (AttachStream::Stdout, b"a\nb".to_vec()),
            (AttachStream::Stderr, b"\nc\n".to_vec()),
        ];

        // Act
        let kept = tail_chunks(&chunks, 2);

        // Assert
        assert_eq!(
            kept,
            vec![
                (AttachStream::Stdout, &b"b"[..]),
                (AttachStream::Stderr, &b"\nc\n"[..]),
            ]
        );
    }

    #[test]
    fn given_output_when_last_lines_taken_then_only_the_tail_remains() {
        // Arrange
        let output = b"a\nb\nc\n";

        // Act and Assert
        assert_eq!(last_lines(output, 2), b"b\nc\n");
        assert_eq!(last_lines(output, 9), output);
        assert_eq!(last_lines(output, 0), b"");
        assert_eq!(last_lines(b"a\nb", 1), b"b");
    }
}
