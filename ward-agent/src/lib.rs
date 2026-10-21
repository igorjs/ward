// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward guest agent library.
//!
//! Runs commands inside a microVM and speaks the length-prefixed protobuf
//! protocol defined in `proto/ward_agent.proto` over a byte stream (vsock in
//! production; any `AsyncRead + AsyncWrite` in tests). One connection carries
//! exactly one process: an opening `Exec`, then `Stdin`/`Kill`/`Signal`/`Resize`
//! frames inbound and `Data` (or legacy line) and `Exited` frames outbound.

use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};

use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

mod pty;

#[allow(clippy::doc_lazy_continuation)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/ward.agent.v1.rs"));
}

use proto::{Data, Event, Exec, Exited, Line, Pong, Request, Stream, event, request};

/// Frames larger than this are rejected as malformed, protecting the agent
/// from a hostile or buggy peer that advertises a huge length.
///
/// SEC-010: lowered from 16 MiB to 2 MiB. The daemon caps broker publish
/// payloads at 1 MiB (validate::MAX_PUBLISH_PAYLOAD_BYTES) and the gRPC
/// decode cap at 1.5 MiB; the agent frame cap mirrors those bounds with
/// protobuf-wrapping headroom. A peer that opens many connections and
/// claims `len = MAX_FRAME_BYTES - 1` on each can no longer force a
/// multi-MiB allocation per connection before the read.
const MAX_FRAME_BYTES: u32 = 2 * 1024 * 1024;

/// Bytes read from a child's output per `Data` event, well under the frame cap.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Capacity of the queue between the output pumps and the single writer task.
const EVENT_QUEUE_DEPTH: usize = 64;

/// Capacity of the queue between the frame loop and the stdin writer task.
const STDIN_QUEUE_DEPTH: usize = 64;

/// How long a tty exec's output may keep draining after the child exits.
const TTY_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Shell convention for "command not found / not executable".
const EXIT_SPAWN_FAILED: i32 = 127;

/// Exit code reported when there is no real exit status (bad request, or a
/// child killed by a signal).
const EXIT_UNKNOWN: i32 = -1;

/// Bit position of the exit code inside a raw wait status.
const WAIT_STATUS_CODE_SHIFT: u32 = 8;

/// Read one length-prefixed frame. Returns `Ok(None)` on a clean EOF at a
/// frame boundary (the peer closed the connection).
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    // SEC-010: reject zero-length frames explicitly. A well-formed
    // protobuf message is never empty; len=0 either indicates a buggy
    // peer or an attacker probing for allocation behaviour. Reject as
    // `InvalidData` (the semantically-correct kind for a malformed
    // framed protocol) so log filters can key off it.
    if len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame length must be non-zero",
        ));
    }
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Write one length-prefixed frame and flush it.
///
/// SEC-010: mirror the reader's len/cap rejection on the write side so
/// the protocol invariant ("frame payload is non-empty and within
/// `MAX_FRAME_BYTES`") is enforced symmetrically. Without this, a buggy
/// future caller constructing `Request::default()` (no `kind` set)
/// would emit a 2-byte header with `len = 0`, and the peer would
/// reject it as malformed; the failure mode would surface as an opaque
/// peer disconnect instead of a local programming error.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame payload must be non-empty",
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame exceeds u32"))?;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("frame too large to write: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

fn decode_request(bytes: &[u8]) -> std::io::Result<Request> {
    Request::decode(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn encode_event(ev: &Event) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ev.encoded_len());
    ev.encode(&mut buf)
        .expect("encoding into a Vec is infallible");
    buf
}

fn line_event(stream: Stream, text: String) -> Event {
    let line = Line { text };
    Event {
        kind: Some(match stream {
            Stream::Stderr => event::Kind::Stderr(line),
            _ => event::Kind::Stdout(line),
        }),
    }
}

fn data_event(stream: Stream, data: Vec<u8>) -> Event {
    Event {
        kind: Some(event::Kind::Data(Data {
            stream: stream as i32,
            data,
        })),
    }
}

fn stderr_event(text: String) -> Event {
    line_event(Stream::Stderr, text)
}

/// Exit event for a failure the agent itself detected: there is no kernel
/// wait status, so the raw status is the one a plain `exit(code)` would give.
fn synthetic_exit_event(code: i32) -> Event {
    exited_event(code, (code & 0xff) << WAIT_STATUS_CODE_SHIFT)
}

fn exited_event(code: i32, wait_status: i32) -> Event {
    Event {
        kind: Some(event::Kind::Exited(Exited { code, wait_status })),
    }
}

/// Handle a single daemon connection: read the opening `Exec`, run the
/// process, and stream its output until it exits.
pub async fn handle_connection<S>(stream: S) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, writer) = tokio::io::split(stream);

    let first = match read_frame(&mut reader).await? {
        Some(f) => f,
        None => return Ok(()), // peer hung up before sending anything
    };
    let exec = match decode_request(&first)?.kind {
        Some(request::Kind::Exec(e)) => e,
        Some(request::Kind::Ping(_)) => {
            let mut writer = writer;
            let pong = Event {
                kind: Some(event::Kind::Pong(Pong {})),
            };
            return write_frame(&mut writer, &encode_event(&pong)).await;
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "first frame must be Exec or Ping",
            ));
        }
    };

    run_process(exec, reader, writer).await
}

/// The child's I/O endpoints, plain pipes or a pty, behind one shape.
struct ChildIo {
    input: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    outputs: Vec<(Stream, Box<dyn AsyncRead + Send + Unpin>)>,
    pty: Option<pty::PtyMaster>,
}

/// Start the child. It always leads its own process group (a pty child, its
/// own session), so signals and kills reach everything it spawns.
fn spawn_child(exec: &Exec) -> std::io::Result<(Child, ChildIo)> {
    let mut cmd = Command::new(&exec.command[0]);
    cmd.args(&exec.command[1..]);
    if !exec.working_dir.is_empty() {
        cmd.current_dir(&exec.working_dir);
    }
    cmd.envs(exec.env.iter());

    if exec.tty {
        let (master, slave) = pty::open()?;
        pty::attach_slave(&mut cmd, slave)?;
        let child = cmd.spawn()?;
        // `cmd` holds the parent's slave copies; drop them so the master
        // sees a hangup when the child side closes.
        drop(cmd);
        let io = ChildIo {
            input: Some(Box::new(master.clone())),
            outputs: vec![(Stream::Stdout, Box::new(master.clone()))],
            pty: Some(master),
        };
        return Ok((child, io));
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let mut outputs: Vec<(Stream, Box<dyn AsyncRead + Send + Unpin>)> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        outputs.push((Stream::Stdout, Box::new(stdout)));
    }
    if let Some(stderr) = child.stderr.take() {
        outputs.push((Stream::Stderr, Box::new(stderr)));
    }
    let io = ChildIo {
        input: child
            .stdin
            .take()
            .map(|s| Box::new(s) as Box<dyn AsyncWrite + Send + Unpin>),
        outputs,
        pty: None,
    };
    Ok((child, io))
}

/// Forward one output stream. Raw mode sends the bytes as read (NULs and
/// partial lines intact). Line mode keeps the legacy `Stdout`/`Stderr` line
/// events for hosts that predate `Data`.
fn pump_output(
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    stream: Stream,
    raw: bool,
    tx: mpsc::Sender<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if raw {
            let mut buf = vec![0u8; READ_CHUNK_BYTES];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0
                    || tx
                        .send(data_event(stream, buf[..n].to_vec()))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        } else {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line_event(stream, line)).await.is_err() {
                    break;
                }
            }
        }
    })
}

/// Write queued stdin to the child on its own task, so a child that stops
/// reading blocks only this task and never the control-frame loop. An empty
/// chunk is an explicit EOF; the queue closing (the host hung up) is not.
fn spawn_stdin_writer(
    mut input: Box<dyn AsyncWrite + Send + Unpin>,
    pty: Option<pty::PtyMaster>,
    mut queue: mpsc::Receiver<Vec<u8>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(chunk) = queue.recv().await {
            if chunk.is_empty() {
                // A pipe sees EOF when `input` drops; a pty has no half-close,
                // so it needs the terminal's EOF character instead.
                if let Some(master) = &pty
                    && let Err(e) = master.send_eof().await
                {
                    tracing::debug!(error = %e, "could not deliver stdin EOF");
                }
                return;
            }
            if input.write_all(&chunk).await.is_err() {
                return;
            }
        }
    })
}

/// Apply one inbound frame to the child.
async fn apply_request(
    kind: request::Kind,
    pid: Option<u32>,
    stdin_tx: &mpsc::Sender<Vec<u8>>,
    pty: Option<&pty::PtyMaster>,
) {
    match kind {
        // A closed queue means the child no longer takes input; drop the data.
        request::Kind::Stdin(s) => {
            let _ = stdin_tx.send(s.data).await;
        }
        request::Kind::Kill(_) => signal_process_group(pid, rustix::process::Signal::KILL),
        request::Kind::Signal(s) => match rustix::process::Signal::from_named_raw(s.signo) {
            Some(sig) => signal_process_group(pid, sig),
            None => tracing::warn!(signo = s.signo, "ignoring unknown signal number"),
        },
        request::Kind::Resize(r) => {
            if let Some(p) = pty
                && let Err(e) = p.set_window_size(r.rows, r.cols)
            {
                tracing::warn!(error = %e, "pty resize failed");
            }
        }
        request::Kind::Exec(_) | request::Kind::Ping(_) => {}
    }
}

/// The `Exited` event for a finished `wait`. A failed wait has no kernel
/// status, so it is reported like any other agent-side failure.
fn exit_event(status: Option<ExitStatus>) -> Event {
    match status {
        Some(s) => exited_event(s.code().unwrap_or(EXIT_UNKNOWN), s.into_raw()),
        None => synthetic_exit_event(EXIT_UNKNOWN),
    }
}

/// Wait for the output pumps. On a pty a background job in another process
/// group can hold the slave open, so the master never reports a hangup; give
/// the pumps a short grace, then stop them.
async fn drain_outputs(tasks: Vec<JoinHandle<()>>, tty: bool) {
    let deadline = tty.then(|| tokio::time::Instant::now() + TTY_DRAIN_GRACE);
    for mut task in tasks {
        match deadline {
            Some(at) => {
                if tokio::time::timeout_at(at, &mut task).await.is_err() {
                    task.abort();
                }
            }
            None => {
                let _ = task.await;
            }
        }
    }
}

async fn run_process<R, W>(exec: Exec, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // One writer task drains events from every producer so frames never
    // interleave on the wire.
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(EVENT_QUEUE_DEPTH);
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(ev) = event_rx.recv().await {
            if write_frame(&mut writer, &encode_event(&ev)).await.is_err() {
                break; // daemon closed the connection; stop writing
            }
        }
    });

    if exec.command.is_empty() {
        let _ = event_tx
            .send(stderr_event("empty command".to_string()))
            .await;
        let _ = event_tx.send(synthetic_exit_event(EXIT_UNKNOWN)).await;
        drop(event_tx);
        let _ = writer_task.await;
        return Ok(());
    }

    let (mut child, io) = match spawn_child(&exec) {
        Ok(spawned) => spawned,
        Err(e) => {
            // Surface the OS error on stderr too.
            let _ = event_tx
                .send(stderr_event(format!("spawn failed: {e}")))
                .await;
            let _ = event_tx.send(synthetic_exit_event(EXIT_SPAWN_FAILED)).await;
            drop(event_tx);
            let _ = writer_task.await;
            return Ok(());
        }
    };
    let pid = child.id();

    // A tty exec is always raw; plain execs stay on line events unless the
    // caller asks for raw bytes, so hosts that predate `Data` keep working.
    let raw = exec.tty || exec.raw;
    let output_tasks: Vec<JoinHandle<()>> = io
        .outputs
        .into_iter()
        .map(|(stream, r)| pump_output(r, stream, raw, event_tx.clone()))
        .collect();

    // Inbound frames: stdin chunks and control requests. Signals target the
    // process group by pid (not `&mut child`) so they never alias the
    // `child.wait()` below.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(STDIN_QUEUE_DEPTH);
    let stdin_writer = io
        .input
        .map(|input| spawn_stdin_writer(input, io.pty.clone(), stdin_rx));
    let pty = io.pty;
    let frame_task = tokio::spawn(async move {
        let mut reader = reader;
        // Ends on a malformed frame, or when the daemon closes its write
        // half. Dropping `stdin_tx` then gives the child EOF but lets the
        // process run to completion; it is not a kill.
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            let Ok(req) = decode_request(&frame) else {
                break;
            };
            if let Some(kind) = req.kind {
                apply_request(kind, pid, &stdin_tx, pty.as_ref()).await;
            }
        }
    });

    let status = child.wait().await.ok();

    // Drain output before announcing exit so every byte precedes Exited.
    drain_outputs(output_tasks, exec.tty).await;
    let _ = event_tx.send(exit_event(status)).await;

    drop(event_tx);
    frame_task.abort();
    if let Some(task) = stdin_writer {
        task.abort();
    }
    let _ = writer_task.await;
    Ok(())
}

/// Deliver `sig` to the child's process group. The child leads its own group
/// (see `spawn_child`), so the group id equals its pid.
fn signal_process_group(pid: Option<u32>, sig: rustix::process::Signal) {
    let Some(pgid) = pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    // A group that already exited returns ESRCH, which is fine to ignore.
    let _ = rustix::process::kill_process_group(pgid, sig);
}

#[cfg(test)]
mod tests {
    use super::proto::Ping;
    use super::*;
    use std::collections::HashMap;
    use tokio::io::AsyncWriteExt;

    /// Encode a daemon-side `Request` for the in-memory transport.
    fn encode_request(req: &Request) -> Vec<u8> {
        let mut buf = Vec::with_capacity(req.encoded_len());
        req.encode(&mut buf).expect("encode");
        buf
    }

    fn exec_req(command: &[&str]) -> Request {
        Request {
            kind: Some(request::Kind::Exec(Exec {
                command: command.iter().map(|s| s.to_string()).collect(),
                working_dir: String::new(),
                env: HashMap::new(),
                ..Default::default()
            })),
        }
    }

    /// Drive `handle_connection` over an in-memory duplex, sending `requests`
    /// and collecting every `Event` until the connection closes.
    async fn run(requests: Vec<Request>) -> Vec<Event> {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let agent_task = tokio::spawn(handle_connection(agent));

        let (mut dr, mut dw) = tokio::io::split(daemon);
        for req in &requests {
            write_frame(&mut dw, &encode_request(req)).await.unwrap();
        }
        // Close the daemon write half so the agent's stdin reader sees EOF
        // once the process has finished consuming any input.
        dw.shutdown().await.unwrap();

        let mut events = Vec::new();
        while let Some(frame) = read_frame(&mut dr).await.unwrap() {
            events.push(Event::decode(&frame[..]).unwrap());
        }
        agent_task.await.unwrap().unwrap();
        events
    }

    fn lines_of(events: &[Event]) -> (Vec<String>, Vec<String>, Option<i32>) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut code = None;
        for e in events {
            match &e.kind {
                Some(event::Kind::Stdout(l)) => out.push(l.text.clone()),
                Some(event::Kind::Stderr(l)) => err.push(l.text.clone()),
                Some(event::Kind::Exited(x)) => code = Some(x.code),
                Some(event::Kind::Pong(_) | event::Kind::Data(_)) | None => {}
            }
        }
        (out, err, code)
    }

    #[tokio::test]
    async fn given_ping_as_first_frame_when_handled_then_agent_answers_pong() {
        let events = run(vec![Request {
            kind: Some(request::Kind::Ping(Ping {})),
        }])
        .await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, Some(event::Kind::Pong(_)))),
            "no Pong in {events:?}"
        );
    }

    #[tokio::test]
    async fn given_echo_when_exec_then_streams_stdout_and_exit_zero() {
        let events = run(vec![exec_req(&["echo", "hello world"])]).await;
        let (out, _err, code) = lines_of(&events);
        assert_eq!(out, vec!["hello world"]);
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    async fn given_nonzero_exit_when_exec_then_reports_code() {
        let events = run(vec![exec_req(&["sh", "-c", "exit 3"])]).await;
        let (_out, _err, code) = lines_of(&events);
        assert_eq!(code, Some(3));
    }

    #[tokio::test]
    async fn given_stderr_writer_when_exec_then_stderr_lines_captured() {
        let events = run(vec![exec_req(&["sh", "-c", "echo oops 1>&2"])]).await;
        let (_out, err, code) = lines_of(&events);
        assert_eq!(err, vec!["oops"]);
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    async fn given_missing_binary_when_exec_then_exit_127() {
        let events = run(vec![exec_req(&["this-command-does-not-exist-xyz"])]).await;
        let (_out, err, code) = lines_of(&events);
        assert_eq!(code, Some(127));
        assert!(
            err.iter().any(|l| l.contains("spawn failed")),
            "expected spawn-failure line, got {err:?}"
        );
    }

    #[tokio::test]
    async fn given_empty_command_when_exec_then_exit_negative_one() {
        let events = run(vec![exec_req(&[])]).await;
        let (_out, _err, code) = lines_of(&events);
        assert_eq!(code, Some(-1));
    }

    #[tokio::test]
    async fn given_stdin_when_cat_then_echoes_back_and_eof_exits() {
        let requests = vec![
            exec_req(&["cat"]),
            Request {
                kind: Some(request::Kind::Stdin(proto::Stdin {
                    data: b"piped line\n".to_vec(),
                })),
            },
            // Empty stdin chunk = EOF, so `cat` terminates.
            Request {
                kind: Some(request::Kind::Stdin(proto::Stdin { data: Vec::new() })),
            },
        ];
        let events = run(requests).await;
        let (out, _err, code) = lines_of(&events);
        assert_eq!(out, vec!["piped line"]);
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    async fn given_long_process_when_kill_then_exits_promptly() {
        // `sleep 30` would hang the test if Kill were ignored; tokio's test
        // runtime gives us no wall clock, but the agent must still terminate
        // the child and report an exit rather than blocking forever.
        let requests = vec![
            exec_req(&["sleep", "30"]),
            Request {
                kind: Some(request::Kind::Kill(proto::Kill {})),
            },
        ];
        let events = run(requests).await;
        let (_out, _err, code) = lines_of(&events);
        // Signal-killed processes have no exit code; the agent reports -1.
        assert_eq!(code, Some(-1));
    }

    // ----- raw reads, pty, signals -------------------------------------------

    fn raw_exec(command: &[&str], tty: bool) -> Request {
        Request {
            kind: Some(request::Kind::Exec(Exec {
                command: command.iter().map(|s| s.to_string()).collect(),
                tty,
                raw: !tty,
                ..Default::default()
            })),
        }
    }

    fn stdin_req(data: &[u8]) -> Request {
        Request {
            kind: Some(request::Kind::Stdin(proto::Stdin {
                data: data.to_vec(),
            })),
        }
    }

    /// Concatenated raw bytes per stream, plus the exit event.
    fn raw_of(events: &[Event]) -> (Vec<u8>, Vec<u8>, Option<Exited>) {
        let (mut out, mut err, mut exited) = (Vec::new(), Vec::new(), None);
        for e in events {
            match &e.kind {
                Some(event::Kind::Data(d)) if d.stream == proto::Stream::Stdout as i32 => {
                    out.extend_from_slice(&d.data);
                }
                Some(event::Kind::Data(d)) if d.stream == proto::Stream::Stderr as i32 => {
                    err.extend_from_slice(&d.data);
                }
                Some(event::Kind::Exited(x)) => exited = Some(*x),
                _ => {}
            }
        }
        (out, err, exited)
    }

    #[tokio::test]
    async fn given_nul_bytes_without_newline_when_raw_then_bytes_arrive_exact() {
        // Arrange
        let requests = vec![raw_exec(&["printf", "a\\0b"], false)];

        // Act
        let events = run(requests).await;

        // Assert
        let (out, _err, exited) = raw_of(&events);
        assert_eq!(out, b"a\0b");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, Some(event::Kind::Stdout(_)))),
            "raw mode must not emit line events"
        );
        assert_eq!(exited.map(|x| x.code), Some(0));
    }

    #[tokio::test]
    async fn given_raw_stderr_partial_line_when_exec_then_tagged_stderr_bytes() {
        let events = run(vec![raw_exec(&["sh", "-c", "printf par 1>&2"], false)]).await;

        let (out, err, _) = raw_of(&events);
        assert!(out.is_empty());
        assert_eq!(err, b"par");
    }

    #[tokio::test]
    async fn given_pty_exec_when_child_checks_isatty_then_all_three_are_ttys() {
        // Arrange
        let script = "test -t 0 && test -t 1 && test -t 2 && printf yes";

        // Act
        let events = run(vec![raw_exec(&["sh", "-c", script], true)]).await;

        // Assert
        let (out, _err, exited) = raw_of(&events);
        assert_eq!(out, b"yes");
        assert_eq!(exited.map(|x| x.code), Some(0));
    }

    #[tokio::test]
    async fn given_pipe_exec_when_child_checks_isatty_then_it_is_not_a_tty() {
        let events = run(vec![raw_exec(
            &["sh", "-c", "test -t 0 || printf no"],
            false,
        )])
        .await;

        let (out, _err, _) = raw_of(&events);
        assert_eq!(out, b"no");
    }

    #[tokio::test]
    async fn given_pty_exec_when_child_inspects_itself_then_it_leads_its_own_session() {
        // Arrange: a session leader has pgid == pid, and a controlling
        // terminal shows up as a tty name in `ps`.
        let script = "ps -o pid= -o pgid= -o tty= -p $$";

        // Act
        let events = run(vec![raw_exec(&["sh", "-c", script], true)]).await;

        // Assert
        let (out, _err, _) = raw_of(&events);
        let text = String::from_utf8_lossy(&out).into_owned();
        let fields: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3, "unexpected ps output {text:?}");
        assert_eq!(fields[0], fields[1], "child is not a group leader");
        assert!(!fields[2].starts_with('?'), "no controlling tty: {text:?}");
    }

    // The child reads /proc, which only exists on Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn given_pty_exec_when_child_reads_proc_stat_then_session_differs_and_tty_is_set() {
        // Arrange
        fn stat_fields(stat: &str) -> Vec<String> {
            // The comm field is parenthesised and may hold spaces; split after it.
            let rest = &stat[stat.rfind(')').unwrap() + 2..];
            rest.split(' ').map(str::to_string).collect()
        }
        // After ')' the fields start at 3 (state), so session is index 3, tty_nr index 4.
        let agent = stat_fields(&std::fs::read_to_string("/proc/self/stat").unwrap());

        // Act
        let events = run(vec![raw_exec(&["cat", "/proc/self/stat"], true)]).await;

        // Assert
        let (out, _err, _) = raw_of(&events);
        let child = stat_fields(String::from_utf8_lossy(&out).trim());
        assert_ne!(child[3], agent[3], "child shares the agent's session");
        assert_ne!(child[4], "0", "child has no controlling terminal");
    }

    #[tokio::test]
    async fn given_resize_when_child_runs_stty_size_then_it_prints_new_dimensions() {
        // Arrange
        let requests = vec![
            raw_exec(&["sh", "-c", "read x; stty size"], true),
            Request {
                kind: Some(request::Kind::Resize(proto::Resize { rows: 40, cols: 80 })),
            },
            stdin_req(b"go\n"),
        ];

        // Act
        let events = run(requests).await;

        // Assert
        let (out, _err, _) = raw_of(&events);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("40 80"), "got {text:?}");
    }

    #[tokio::test]
    async fn given_pty_stdin_when_cat_then_input_is_echoed_through_the_terminal() {
        let requests = vec![
            raw_exec(&["sh", "-c", "read x; printf got:$x"], true),
            stdin_req(b"hi\n"),
        ];

        let events = run(requests).await;

        let (out, _err, _) = raw_of(&events);
        assert!(String::from_utf8_lossy(&out).contains("got:hi"));
    }

    #[tokio::test]
    async fn given_empty_kill_when_child_runs_then_status_is_sigkill() {
        let requests = vec![
            raw_exec(&["sleep", "30"], false),
            Request {
                kind: Some(request::Kind::Kill(proto::Kill {})),
            },
        ];

        let events = run(requests).await;

        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.wait_status), Some(0x0009));
    }

    #[test]
    fn given_failed_wait_when_exit_event_built_then_status_matches_synthetic_unknown() {
        // Arrange
        let expected = synthetic_exit_event(EXIT_UNKNOWN);

        // Act
        let event = exit_event(None);

        // Assert
        assert_eq!(event, expected);
        let Some(event::Kind::Exited(x)) = event.kind else {
            panic!("expected Exited");
        };
        assert_eq!(
            x.wait_status,
            (EXIT_UNKNOWN & 0xff) << WAIT_STATUS_CODE_SHIFT
        );
    }

    #[tokio::test]
    async fn given_signal_term_when_child_runs_then_status_is_sigterm() {
        let requests = vec![
            raw_exec(&["sleep", "30"], false),
            Request {
                kind: Some(request::Kind::Signal(proto::Signal { signo: 15 })),
            },
        ];

        let events = run(requests).await;

        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.wait_status), Some(0x000F));
    }

    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const SIGTERM: i32 = 15;
    const WAIT_STATUS_SIGTERM: i32 = 0x000F;
    const STDIN_FRAME_BYTES: usize = 1024 * 1024;
    const STDIN_FRAME_COUNT: usize = 4;

    /// Like `run`, but the daemon write half stays open until the agent is
    /// done, so only an explicit request (not a hangup) can end the child.
    async fn run_held_open(requests: Vec<Request>) -> Vec<Event> {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let agent_task = tokio::spawn(handle_connection(agent));
        let (mut dr, mut dw) = tokio::io::split(daemon);
        for req in &requests {
            write_frame(&mut dw, &encode_request(req)).await.unwrap();
        }
        let mut events = Vec::new();
        while let Some(frame) = read_frame(&mut dr).await.unwrap() {
            events.push(Event::decode(&frame[..]).unwrap());
        }
        agent_task.await.unwrap().unwrap();
        events
    }

    /// Send `exec`, wait until the child prints `marker`, then send `then`.
    async fn run_after_marker(exec: Request, marker: &[u8], then: Request) -> Vec<Event> {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let agent_task = tokio::spawn(handle_connection(agent));
        let (mut dr, mut dw) = tokio::io::split(daemon);
        write_frame(&mut dw, &encode_request(&exec)).await.unwrap();
        let mut events = Vec::new();
        let mut sent = false;
        while let Some(frame) = read_frame(&mut dr).await.unwrap() {
            events.push(Event::decode(&frame[..]).unwrap());
            if !sent && raw_of(&events).0.windows(marker.len()).any(|w| w == marker) {
                write_frame(&mut dw, &encode_request(&then)).await.unwrap();
                sent = true;
            }
        }
        agent_task.await.unwrap().unwrap();
        events
    }

    fn signal_req(signo: i32) -> Request {
        Request {
            kind: Some(request::Kind::Signal(proto::Signal { signo })),
        }
    }

    #[tokio::test]
    async fn given_signal_when_child_has_a_grandchild_then_the_whole_group_gets_it() {
        // Arrange: the shell announces the background job, then waits on it;
        // the signal is sent only once the job exists, so a signal to the
        // shell pid alone would leave the sleep running.
        let exec = raw_exec(&["sh", "-c", "sleep 30 & echo ready; wait"], false);

        // Act
        let events = tokio::time::timeout(
            TEST_TIMEOUT,
            run_after_marker(exec, b"ready", signal_req(SIGTERM)),
        )
        .await
        .expect("signal did not reach the process group");

        // Assert
        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.wait_status), Some(WAIT_STATUS_SIGTERM));
    }

    #[tokio::test]
    async fn given_tty_exec_when_background_job_holds_the_pty_then_exited_still_arrives() {
        // Arrange: the sleep keeps the pty slave open after the shell exits.
        let requests = vec![raw_exec(&["sh", "-c", "sleep 30 & exit 3"], true)];

        // Act
        let events = tokio::time::timeout(TEST_TIMEOUT, run_held_open(requests))
            .await
            .expect("Exited never arrived");

        // Assert
        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.code), Some(3));
    }

    #[tokio::test]
    async fn given_child_not_reading_stdin_when_signal_sent_then_it_is_still_handled() {
        // Arrange: far more stdin than a pipe holds, then SIGTERM.
        let payload = vec![b'x'; STDIN_FRAME_BYTES];
        let mut requests = vec![raw_exec(&["sleep", "30"], false)];
        requests.extend((0..STDIN_FRAME_COUNT).map(|_| stdin_req(&payload)));
        requests.push(signal_req(SIGTERM));

        // Act
        let events = tokio::time::timeout(TEST_TIMEOUT, run_held_open(requests))
            .await
            .expect("signal was starved by blocked stdin");

        // Assert
        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.wait_status), Some(WAIT_STATUS_SIGTERM));
    }

    #[tokio::test]
    async fn given_tty_exec_when_empty_stdin_sent_then_cat_sees_eof_and_exits() {
        // Arrange: the host keeps its write half open, so only the explicit
        // empty Stdin can deliver EOF.
        let requests = vec![raw_exec(&["cat"], true), stdin_req(b"")];

        // Act
        let events = tokio::time::timeout(TEST_TIMEOUT, run_held_open(requests))
            .await
            .expect("cat never saw EOF");

        // Assert
        let (_o, _e, exited) = raw_of(&events);
        assert_eq!(exited.map(|x| x.code), Some(0));
    }

    // ----- SEC-010 frame invariants ----------------------------------------

    #[tokio::test]
    async fn given_zero_length_header_when_read_frame_then_invalid_data() {
        // Arrange: write only the 4-byte length prefix with len=0.
        let (mut client, mut agent) = tokio::io::duplex(8);
        client.write_all(&0u32.to_be_bytes()).await.unwrap();
        client.shutdown().await.unwrap();

        // Act
        let err = read_frame(&mut agent)
            .await
            .expect_err("len=0 must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("non-zero"));
    }

    #[tokio::test]
    async fn given_over_cap_length_header_when_read_frame_then_invalid_data() {
        // Arrange
        let (mut client, mut agent) = tokio::io::duplex(8);
        let oversized = MAX_FRAME_BYTES + 1;
        client.write_all(&oversized.to_be_bytes()).await.unwrap();
        client.shutdown().await.unwrap();

        // Act
        let err = read_frame(&mut agent)
            .await
            .expect_err("over-cap must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn given_empty_payload_when_write_frame_then_invalid_input() {
        // Arrange: write side mirrors the read-side invariant. Without this
        // a future caller that builds `Request::default()` (no kind set,
        // encoded_len() = 0) could ship a malformed frame the peer would
        // reject as an opaque disconnect.
        let mut sink: Vec<u8> = Vec::new();

        // Act
        let err = write_frame(&mut sink, &[])
            .await
            .expect_err("empty payload must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Nothing was written to the sink because the guard fires before
        // the length-prefix write.
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn given_over_cap_payload_when_write_frame_then_invalid_input() {
        // Arrange: synthesise a too-large payload without actually allocating
        // MAX_FRAME_BYTES (cheap: the cap check fires before write_all).
        let mut sink: Vec<u8> = Vec::new();
        let oversize = vec![0u8; (MAX_FRAME_BYTES as usize) + 1];

        // Act
        let err = write_frame(&mut sink, &oversize)
            .await
            .expect_err("over-cap payload must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn given_non_exec_first_frame_when_handle_then_errors() {
        let (daemon, agent) = tokio::io::duplex(1024);
        let agent_task = tokio::spawn(handle_connection(agent));
        let (_dr, mut dw) = tokio::io::split(daemon);
        let bad = Request {
            kind: Some(request::Kind::Kill(proto::Kill {})),
        };
        write_frame(&mut dw, &encode_request(&bad)).await.unwrap();
        dw.shutdown().await.unwrap();
        let result = agent_task.await.unwrap();
        assert!(result.is_err(), "Kill as the first frame must be rejected");
    }
}

#[cfg(target_os = "linux")]
pub mod net;

/// A host directory shared into the guest over virtiofs, to be mounted at
/// `target` when the agent starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestMount {
    pub tag: String,
    pub target: String,
    pub read_only: bool,
}

/// Argument that introduces one mount: `--mount <tag> <target> <ro|rw>`.
pub const MOUNT_ARG: &str = "--mount";

/// Parse the agent's command-line arguments (without the program name).
///
/// # Errors
///
/// Fails on an unknown argument, a truncated `--mount`, or a mode other than
/// `ro` or `rw`.
pub fn parse_mount_args<I: IntoIterator<Item = String>>(
    args: I,
) -> Result<Vec<GuestMount>, String> {
    let mut args = args.into_iter();
    let mut mounts = Vec::new();
    while let Some(arg) = args.next() {
        if arg != MOUNT_ARG {
            return Err(format!("unknown argument {arg:?}"));
        }
        let (Some(tag), Some(target), Some(mode)) = (args.next(), args.next(), args.next()) else {
            return Err("--mount needs <tag> <target> <ro|rw>".to_string());
        };
        let read_only = match mode.as_str() {
            "ro" => true,
            "rw" => false,
            other => return Err(format!("mount mode must be ro or rw, got {other:?}")),
        };
        mounts.push(GuestMount {
            tag,
            target,
            read_only,
        });
    }
    Ok(mounts)
}

/// Mount every share in `mounts`, creating the target directories first.
///
/// # Errors
///
/// Fails on the first mount that the kernel refuses.
#[cfg(target_os = "linux")]
pub fn mount_all(mounts: &[GuestMount]) -> std::io::Result<()> {
    use rustix::mount::{MountFlags, mount};
    for m in mounts {
        std::fs::create_dir_all(&m.target)?;
        let flags = if m.read_only {
            MountFlags::RDONLY
        } else {
            MountFlags::empty()
        };
        mount(m.tag.as_str(), m.target.as_str(), "virtiofs", flags, None)
            .map_err(|e| std::io::Error::other(format!("mount {} at {}: {e}", m.tag, m.target)))?;
    }
    Ok(())
}

#[cfg(test)]
mod mount_arg_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn given_no_args_when_parsed_then_no_mounts() {
        assert_eq!(parse_mount_args(args(&[])), Ok(vec![]));
    }

    #[test]
    fn given_two_mounts_when_parsed_then_both_returned_in_order() {
        let parsed = parse_mount_args(args(&[
            "--mount", "t0", "/data", "rw", "--mount", "t1", "/cfg dir", "ro",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                GuestMount {
                    tag: "t0".into(),
                    target: "/data".into(),
                    read_only: false
                },
                GuestMount {
                    tag: "t1".into(),
                    target: "/cfg dir".into(),
                    read_only: true
                },
            ]
        );
    }

    #[test]
    fn given_truncated_mount_when_parsed_then_error() {
        assert!(parse_mount_args(args(&["--mount", "t0", "/data"])).is_err());
    }

    #[test]
    fn given_bad_mode_when_parsed_then_error() {
        assert!(parse_mount_args(args(&["--mount", "t0", "/d", "rwx"])).is_err());
    }

    #[test]
    fn given_unknown_flag_when_parsed_then_error() {
        assert!(parse_mount_args(args(&["--bogus"])).is_err());
    }
}

#[cfg(test)]
mod proto_compat_tests {
    use super::*;

    const WAIT_STATUS_SIGKILL: i32 = 0x0009;
    const TEST_ROWS: u32 = 40;
    const TEST_COLS: u32 = 80;
    const SIGTERM_NO: i32 = 15;

    fn round_trip<M: Message + Default>(msg: &M) -> M {
        M::decode(&msg.encode_to_vec()[..]).expect("decode")
    }

    #[test]
    fn given_old_line_events_when_new_events_exist_then_old_events_still_parse() {
        // Arrange
        let old = [
            line_event(Stream::Stdout, "out".into()),
            stderr_event("err".into()),
        ];
        let data = Event {
            kind: Some(event::Kind::Data(proto::Data {
                stream: proto::Stream::Stdout as i32,
                data: vec![0, 1, 0],
            })),
        };

        // Act
        let decoded: Vec<Event> = old.iter().map(round_trip).collect();
        let data_back = round_trip(&data);

        // Assert
        assert_eq!(decoded[0], old[0]);
        assert_eq!(decoded[1], old[1]);
        assert_eq!(data_back, data);
    }

    #[test]
    fn given_empty_kill_when_round_tripped_then_it_equals_empty_kill() {
        // Arrange
        let req = Request {
            kind: Some(request::Kind::Kill(proto::Kill {})),
        };

        // Act
        let back = round_trip(&req);

        // Assert
        assert_eq!(back, req);
    }

    #[test]
    fn given_clean_and_sigkill_wait_status_when_decoded_then_they_differ() {
        // Arrange
        let clean = exited_event(0, 0);
        let killed = Event {
            kind: Some(event::Kind::Exited(Exited {
                code: -1,
                wait_status: WAIT_STATUS_SIGKILL,
            })),
        };

        // Act
        let (clean, killed) = (round_trip(&clean), round_trip(&killed));

        // Assert
        let status = |e: Event| match e.kind {
            Some(event::Kind::Exited(x)) => x.wait_status,
            other => panic!("expected Exited, got {other:?}"),
        };
        assert_eq!(status(clean), 0);
        assert_eq!(status(killed), WAIT_STATUS_SIGKILL);
    }

    #[test]
    fn given_resize_and_signal_requests_when_round_tripped_then_fields_survive() {
        // Arrange
        let resize = Request {
            kind: Some(request::Kind::Resize(proto::Resize {
                rows: TEST_ROWS,
                cols: TEST_COLS,
            })),
        };
        let signal = Request {
            kind: Some(request::Kind::Signal(proto::Signal { signo: SIGTERM_NO })),
        };

        // Act
        let (resize_back, signal_back) = (round_trip(&resize), round_trip(&signal));

        // Assert
        assert_eq!(resize_back, resize);
        assert_eq!(signal_back, signal);
    }

    #[test]
    fn given_exec_with_tty_and_user_when_round_tripped_then_fields_survive() {
        // Arrange
        let exec = Exec {
            command: vec!["sh".into()],
            tty: true,
            user: "1000:1000".into(),
            ..Default::default()
        };

        // Act
        let back = round_trip(&exec);

        // Assert
        assert!(back.tty);
        assert_eq!(back.user, "1000:1000");
    }
}
