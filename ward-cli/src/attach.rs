// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Client side of the Attach stream: sends the start message first, then
//! forwards stdin, resize and signal messages while printing the output.

use std::io::{Read, Write};
use std::os::fd::AsFd;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rustix::termios::isatty;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;
use ward_core::pb::attach_message::Kind;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    AttachMessage, AttachResize, AttachSignal, AttachStart, AttachStdin, AttachStream,
};

use crate::client::Resolved;
use crate::terminal::{self, ExitSignals, RawModeGuard};

/// Docker's wording, so scripts that match on it keep working.
const NOT_A_TTY: &str = "the input device is not a TTY";
const STDIN_CHUNK_BYTES: usize = 8192;
const OUTBOUND_CAPACITY: usize = 16;

/// A piece of output with the stream it came from and, when the daemon sent
/// one, the time it arrived.
pub struct Chunk<'a> {
    pub stream: AttachStream,
    pub bytes: &'a [u8],
    pub at: Option<DateTime<Utc>>,
}

/// What an attach stream hands to its sink.
pub enum Output<'a> {
    Data(Chunk<'a>),
    /// The buffered output is complete; anything after is live.
    ReplayEnd,
}

/// Fails when stdin is attached with a tty (`-it`) but is not a terminal.
/// Like Docker, `-t` alone does not need a terminal on stdin.
pub fn require_terminal(tty: bool, interactive: bool) -> Result<()> {
    if tty && interactive && !isatty(std::io::stdin()) {
        bail!(NOT_A_TTY);
    }
    Ok(())
}

/// The exit code a finished attach reported, or an error when the stream
/// closed without one.
pub fn require_exit(code: Option<i32>) -> Result<i32> {
    code.context("the attach stream ended without an exit status")
}

/// Streams one process to the terminal and returns its exit code, or `None`
/// when the stream ended without one (a replay of a running process).
///
/// A SIGHUP ends the stream with `128 + signo`. So do SIGINT and SIGTERM
/// when the CLI owns the terminal (raw mode) or only watches (replay);
/// otherwise they go to the guest process.
pub async fn attach(
    client: &mut WardClient<Channel>,
    target: &Resolved,
    start: AttachStart,
    forward_stdin: bool,
) -> Result<Option<i32>> {
    let sink = |output: Output| match output {
        Output::Data(chunk) => write_to_terminal(chunk.stream, chunk.bytes),
        Output::ReplayEnd => Ok(()),
    };
    attach_with(client, target, start, forward_stdin, sink).await
}

/// Writes a chunk to stdout, or to stderr when the process wrote it there.
pub fn write_to_terminal(stream: AttachStream, bytes: &[u8]) -> std::io::Result<()> {
    write_to(
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        stream,
        bytes,
    )
}

fn write_to(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    stream: AttachStream,
    bytes: &[u8],
) -> std::io::Result<()> {
    match stream {
        AttachStream::Stdout => write_flushed(stdout, bytes),
        AttachStream::Stderr => write_flushed(stderr, bytes),
    }
}

fn write_flushed(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(bytes)?;
    out.flush()
}

/// Like [`attach`], handing every output chunk and the replay end to `sink`.
pub async fn attach_with(
    client: &mut WardClient<Channel>,
    target: &Resolved,
    start: AttachStart,
    forward_stdin: bool,
    sink: impl FnMut(Output) -> std::io::Result<()>,
) -> Result<Option<i32>> {
    // Registered first so a signal during setup cannot leave raw mode on.
    let mut exits = ExitSignals::register(!relays_signals(&start, forward_stdin))?;
    attach_with_exits(client, target, start, forward_stdin, &mut exits, sink).await
}

/// Like [`attach`], using handlers the caller registered with SIGINT and
/// SIGTERM included, so the caller keeps hearing them once the attach ends.
pub async fn attach_registered(
    client: &mut WardClient<Channel>,
    target: &Resolved,
    start: AttachStart,
    forward_stdin: bool,
    exits: &mut ExitSignals,
) -> Result<Option<i32>> {
    let sink = |output: Output| match output {
        Output::Data(chunk) => write_to_terminal(chunk.stream, chunk.bytes),
        Output::ReplayEnd => Ok(()),
    };
    attach_with_exits(client, target, start, forward_stdin, exits, sink).await
}

/// SIGINT and SIGTERM go to the guest, unless the CLI owns the terminal
/// (raw mode) or only watches (replay).
fn relays_signals(start: &AttachStart, forward_stdin: bool) -> bool {
    !(start.tty && forward_stdin) && !start.replay
}

async fn attach_with_exits(
    client: &mut WardClient<Channel>,
    target: &Resolved,
    start: AttachStart,
    forward_stdin: bool,
    exits: &mut ExitSignals,
    mut sink: impl FnMut(Output) -> std::io::Result<()>,
) -> Result<Option<i32>> {
    let tty = start.tty;
    let replay = start.replay;
    // Raw mode turns Ctrl-C into a byte, so it is only worth it when
    // stdin is forwarded. Dropped on every exit path, which restores it.
    let raw_input = tty && forward_stdin;
    let relay_signals = relays_signals(&start, forward_stdin);
    let _raw = if raw_input {
        Some(enter_raw_mode()?)
    } else {
        None
    };

    let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
    send(&tx, Kind::Start(start)).await?;
    if tty {
        forward_resizes(tx.clone())?;
    }
    if relay_signals {
        forward_signals(tx.clone())?;
    }
    if forward_stdin {
        forward_stdin_bytes(tx.clone());
    } else if !replay {
        // Nothing will be typed, so tell the process its stdin is closed.
        send(&tx, Kind::Stdin(AttachStdin { data: Vec::new() })).await?;
    }

    let request = target.request(ReceiverStream::new(rx))?;
    let streaming = async {
        let replies = client.attach(request).await?.into_inner();
        relay_replies(replies, replay, &mut sink).await
    };
    // `tx` lives to here, so the client half stays open until the end.
    let outcome = tokio::select! {
        outcome = streaming => outcome,
        code = recv_exit(exits, relay_signals) => Ok(Some(code)),
    };
    drop(tx);
    if relay_signals {
        // Already relayed to the guest, not meant for the caller's next wait.
        exits.discard_pending_interrupts();
    }
    outcome
}

async fn recv_exit(exits: &mut ExitSignals, relay_signals: bool) -> i32 {
    if relay_signals {
        exits.recv_hangup().await
    } else {
        exits.recv_exit_code().await
    }
}

/// Hands every output chunk to `sink` until the stream ends or reports an
/// exit code.
async fn relay_replies(
    mut replies: impl Stream<Item = Result<AttachMessage, tonic::Status>> + Unpin,
    replay: bool,
    sink: &mut impl FnMut(Output) -> std::io::Result<()>,
) -> Result<Option<i32>> {
    while let Some(reply) = replies.next().await {
        match reply?.kind {
            Some(Kind::Data(data)) => {
                // An unknown value, from a newer server, reads as stdout.
                let stream = AttachStream::try_from(data.stream).unwrap_or_default();
                let at = data.timestamp.and_then(|t| {
                    DateTime::from_timestamp(t.seconds, u32::try_from(t.nanos).unwrap_or_default())
                });
                sink(Output::Data(Chunk {
                    stream,
                    bytes: &data.data,
                    at,
                }))?;
            }
            Some(Kind::ReplayEnd(_)) => sink(Output::ReplayEnd)?,
            // A log follower reports the output, not the guest's exit code.
            Some(Kind::Exit(exit)) => return Ok((!replay).then_some(exit.exit_code)),
            _ => {}
        }
    }
    Ok(None)
}

fn enter_raw_mode() -> Result<RawModeGuard> {
    let stdin = std::io::stdin();
    RawModeGuard::enable(stdin.as_fd()).context("could not put the terminal in raw mode")
}

async fn send(tx: &mpsc::Sender<AttachMessage>, kind: Kind) -> Result<()> {
    tx.send(AttachMessage { kind: Some(kind) })
        .await
        .context("the attach stream closed")
}

/// Sends the current terminal size now and again on every SIGWINCH.
fn forward_resizes(tx: mpsc::Sender<AttachMessage>) -> Result<()> {
    let mut winch = signal(SignalKind::window_change())?;
    tokio::spawn(async move {
        loop {
            if let Ok(size) = terminal::window_size(std::io::stdin()) {
                let resize = AttachResize {
                    rows: size.rows.into(),
                    cols: size.cols.into(),
                };
                if send(&tx, Kind::Resize(resize)).await.is_err() {
                    return;
                }
            }
            if winch.recv().await.is_none() {
                return;
            }
        }
    });
    Ok(())
}

/// When the CLI does not own the terminal, Ctrl-C reaches the CLI itself,
/// so it hands SIGINT and SIGTERM to the guest process.
fn forward_signals(tx: mpsc::Sender<AttachMessage>) -> Result<()> {
    for kind in [SignalKind::interrupt(), SignalKind::terminate()] {
        let signo = kind.as_raw_value();
        let mut stream: Signal = signal(kind)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            while stream.recv().await.is_some() {
                if send(&tx, Kind::Signal(AttachSignal { signo }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }
    Ok(())
}

/// Copies stdin to the stream, then sends the empty message that means EOF.
///
/// Runs on its own thread: a blocking read on the runtime would stall its
/// shutdown on the error path.
fn forward_stdin_bytes(tx: mpsc::Sender<AttachMessage>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; STDIN_CHUNK_BYTES];
        loop {
            let read = match stdin.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other.unwrap_or(0),
            };
            let data = buf[..read].to_vec();
            let message = AttachMessage {
                kind: Some(Kind::Stdin(AttachStdin { data })),
            };
            if tx.blocking_send(message).is_err() || read == 0 {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ward_core::pb::{AttachData, AttachExit};

    fn data(stream: AttachStream, text: &str) -> Result<AttachMessage, tonic::Status> {
        Ok(AttachMessage {
            kind: Some(Kind::Data(AttachData {
                stream: stream as i32,
                data: text.as_bytes().to_vec(),
                timestamp: None,
            })),
        })
    }

    #[tokio::test]
    async fn given_mixed_replies_when_relayed_then_each_stream_reaches_its_writer() {
        // Arrange
        let exit = Ok(AttachMessage {
            kind: Some(Kind::Exit(AttachExit { exit_code: 3 })),
        });
        let replies = tokio_stream::iter(vec![
            data(AttachStream::Stdout, "out1"),
            data(AttachStream::Stderr, "err1"),
            data(AttachStream::Stdout, "out2"),
            exit,
        ]);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut sink = |output: Output| match output {
            Output::Data(c) => write_to(&mut stdout, &mut stderr, c.stream, c.bytes),
            Output::ReplayEnd => Ok(()),
        };

        // Act
        let code = relay_replies(replies, false, &mut sink).await.unwrap();

        // Assert
        assert_eq!(code, Some(3));
        assert_eq!(stdout, b"out1out2");
        assert_eq!(stderr, b"err1");
    }
}
