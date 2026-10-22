// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Daemon-side client for the guest agent.
//!
//! Bridges one agent vsock connection to the `ProcessHandle` channels the
//! `SandboxManager` expects: outbound `Stdin`/`Kill` frames are produced from
//! the manager's `stdin_tx` and kill signal; inbound `Stdout`/`Stderr`/
//! `Exited` frames become `StreamEvent`s on `output_rx`.
//!
//! The transport is generic (`AsyncRead + AsyncWrite`) so this is fully
//! exercised over an in-memory duplex in tests; production passes a vsock
//! stream to a booted microVM.

use std::collections::HashMap;

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{ExecMode, ProcessControl, ProcessHandle};
use crate::protocol::{RawStream, StreamEvent, StreamEventKind};

/// Agent protocol types, generated from `proto/ward_agent.proto`.
pub mod proto {
    tonic::include_proto!("ward.agent.v1");
}

use proto::{Event, Exec, Request, event, request};

/// SEC-010: lowered from 16 MiB to 2 MiB to match the daemon's broker
/// publish-payload cap (1 MiB) plus protobuf-wrapping headroom. The
/// previous 16 MiB cap allowed a hostile guest agent (or a compromised
/// vsock bridge) to force a 16 MiB allocation per connection before
/// any payload arrived.
const MAX_FRAME_BYTES: u32 = 2 * 1024 * 1024;

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    // SEC-010: reject zero-length frames. Well-formed protobuf is never
    // empty; len=0 is either a buggy peer or an attacker probe. Use
    // `InvalidData` (not `Other`) so the error kind matches the
    // semantic category and log filters can route on it.
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

/// SEC-010: mirror the reader's len/cap rejection on the write side so
/// the protocol invariant holds symmetrically. Without this the daemon
/// could ship a malformed `len = 0` frame (from a future `Request::default()`
/// caller) to the guest, where the agent's reader would reject it as
/// `InvalidData` and surface the failure as an opaque peer disconnect.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
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

pub(crate) fn encode<M: Message>(m: &M) -> Vec<u8> {
    let mut buf = Vec::with_capacity(m.encoded_len());
    m.encode(&mut buf)
        .expect("encoding into a Vec is infallible");
    buf
}

fn now() -> std::time::SystemTime {
    std::time::SystemTime::now()
}

/// Buffered control requests per process; resize and signal are rare.
const CONTROL_CHANNEL_CAPACITY: usize = 8;

fn control_request(ctl: ProcessControl) -> request::Kind {
    match ctl {
        ProcessControl::Resize { rows, cols } => {
            request::Kind::Resize(proto::Resize { rows, cols })
        }
        ProcessControl::Signal(signo) => request::Kind::Signal(proto::Signal { signo }),
    }
}

fn raw_stream(tag: i32) -> Option<RawStream> {
    match proto::Stream::try_from(tag) {
        Ok(proto::Stream::Stdout) => Some(RawStream::Stdout),
        Ok(proto::Stream::Stderr) => Some(RawStream::Stderr),
        _ => None,
    }
}

/// The raw wait status, or `None` when the agent did not report one. An agent
/// that predates the field leaves it 0 (proto3 default) even for a failed
/// exit, which must not read as a clean status.
fn raw_wait_status(code: i32, wait_status: i32) -> Option<i32> {
    (wait_status != 0 || code == 0).then_some(wait_status)
}

/// Drive one process over `stream`: send the opening `Exec`, then bridge the
/// connection to `ProcessHandle` channels. Returns the handle plus a kill
/// sender the backend stores so `kill_process` can target this connection.
pub async fn drive_exec<S>(
    stream: S,
    pid: String,
    sandbox_id: String,
    command: Vec<String>,
    working_dir: Option<String>,
    env: HashMap<String, String>,
    mode: ExecMode,
) -> std::io::Result<(ProcessHandle, mpsc::Sender<()>)>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    let exec = Request {
        kind: Some(request::Kind::Exec(Exec {
            command,
            working_dir: working_dir.unwrap_or_default(),
            env,
            tty: mode.tty,
            raw: mode.raw,
            ..Default::default()
        })),
    };
    write_frame(&mut writer, &encode(&exec)).await?;

    let (output_tx, output_rx) = mpsc::channel::<StreamEvent>(64);
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<bytes::Bytes>(64);
    let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);
    let (control_tx, mut control_rx) = mpsc::channel::<ProcessControl>(CONTROL_CHANNEL_CAPACITY);

    // Outbound: forward stdin chunks, control requests and a single kill, framing each.
    tokio::spawn(async move {
        let mut stdin_open = true;
        let mut control_open = true;
        loop {
            tokio::select! {
                chunk = stdin_rx.recv(), if stdin_open => match chunk {
                    Some(bytes) => {
                        let req = Request {
                            kind: Some(request::Kind::Stdin(proto::Stdin { data: bytes.to_vec() })),
                        };
                        if write_frame(&mut writer, &encode(&req)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // Manager dropped stdin_tx: signal EOF to the child,
                        // then stop polling stdin (but stay for a kill).
                        let req = Request {
                            kind: Some(request::Kind::Stdin(proto::Stdin { data: Vec::new() })),
                        };
                        let _ = write_frame(&mut writer, &encode(&req)).await;
                        stdin_open = false;
                    }
                },
                ctl = control_rx.recv(), if control_open => match ctl {
                    Some(ctl) => {
                        let req = Request { kind: Some(control_request(ctl)) };
                        if write_frame(&mut writer, &encode(&req)).await.is_err() {
                            break;
                        }
                    }
                    None => control_open = false,
                },
                k = kill_rx.recv() => {
                    if k.is_some() {
                        let req = Request { kind: Some(request::Kind::Kill(proto::Kill {})) };
                        let _ = write_frame(&mut writer, &encode(&req)).await;
                    }
                    break; // kill sent, or backend dropped the sender
                }
            }
        }
    });

    // Inbound: translate agent events into StreamEvents.
    let started = now();
    tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            let Ok(ev) = Event::decode(&frame[..]) else {
                break;
            };
            let event = match ev.kind {
                Some(event::Kind::Stdout(l)) => StreamEvent {
                    kind: StreamEventKind::Stdout,
                    line: l.text,
                    exit_code: None,
                    wait_status: None,
                    timestamp: now(),
                    duration_ms: 0,
                },
                Some(event::Kind::Stderr(l)) => StreamEvent {
                    kind: StreamEventKind::Stderr,
                    line: l.text,
                    exit_code: None,
                    wait_status: None,
                    timestamp: now(),
                    duration_ms: 0,
                },
                Some(event::Kind::Data(d)) => {
                    // An unknown stream tag from a newer agent is dropped.
                    let Some(stream) = raw_stream(d.stream) else {
                        continue;
                    };
                    StreamEvent {
                        kind: StreamEventKind::Data {
                            stream,
                            bytes: d.data,
                        },
                        line: String::new(),
                        exit_code: None,
                        wait_status: None,
                        timestamp: now(),
                        duration_ms: 0,
                    }
                }
                Some(event::Kind::Exited(x)) => {
                    let _ = output_tx
                        .send(StreamEvent {
                            kind: StreamEventKind::Exit,
                            line: String::new(),
                            exit_code: Some(x.code),
                            wait_status: raw_wait_status(x.code, x.wait_status),
                            timestamp: now(),
                            duration_ms: started
                                .elapsed()
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0),
                        })
                        .await;
                    break; // terminal event; closing output_tx ends the stream
                }
                _ => continue,
            };
            if output_tx.send(event).await.is_err() {
                break; // consumer gone
            }
        }
    });

    let handle = ProcessHandle {
        pid,
        sandbox_id,
        stdin_tx: Some(stdin_tx),
        output_rx: Some(output_rx),
        control_tx: Some(control_tx),
    };
    Ok((handle, kill_tx))
}

/// Check that the guest agent is up: send `Ping` and expect `Pong`.
///
/// # Errors
///
/// Returns an error if the connection closes early or the reply is not `Pong`.
pub async fn ping<S>(mut stream: S) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = Request {
        kind: Some(request::Kind::Ping(proto::Ping {})),
    };
    write_frame(&mut stream, &encode(&req)).await?;
    let frame = read_frame(&mut stream).await?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "agent closed the connection before replying",
        )
    })?;
    match Event::decode(&frame[..]) {
        Ok(Event {
            kind: Some(event::Kind::Pong(_)),
        }) => Ok(()),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "agent reply to Ping was not Pong",
        )),
    }
}

/// Bytes of tar in one `ArchiveChunk` frame, well under the frame cap.
pub const ARCHIVE_CHUNK_BYTES: usize = 1024 * 1024;

fn protocol_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_string())
}

fn agent_failure(message: String) -> std::io::Error {
    std::io::Error::other(format!("guest agent: {message}"))
}

/// Send `chunks` as one tar stream to unpack into the guest directory `path`.
///
/// # Errors
///
/// Returns an error if the agent refuses the tar or the connection fails.
pub async fn put_archive<S>(
    stream: S,
    path: String,
    mut chunks: mpsc::Receiver<bytes::Bytes>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let open = Request {
        kind: Some(request::Kind::PutArchive(proto::PutArchive { path })),
    };
    write_frame(&mut writer, &encode(&open)).await?;
    while let Some(chunk) = chunks.recv().await {
        for piece in chunk.chunks(ARCHIVE_CHUNK_BYTES) {
            let frame = Request {
                kind: Some(request::Kind::ArchiveChunk(proto::ArchiveChunk {
                    data: piece.to_vec(),
                })),
            };
            write_frame(&mut writer, &encode(&frame)).await?;
        }
    }
    let end = Request {
        kind: Some(request::Kind::ArchiveEnd(proto::ArchiveEnd {})),
    };
    write_frame(&mut writer, &encode(&end)).await?;
    let frame = read_frame(&mut reader)
        .await?
        .ok_or_else(|| protocol_error("agent closed the connection before answering"))?;
    match Event::decode(&frame[..]).map(|e| e.kind) {
        Ok(Some(event::Kind::ArchiveDone(_))) => Ok(()),
        Ok(Some(event::Kind::ArchiveError(e))) => Err(agent_failure(e.message)),
        _ => Err(protocol_error(
            "agent answered a PutArchive with another event",
        )),
    }
}

/// Ask the agent for a tar of the guest `path` and pass its chunks to `out`.
/// A failure ends the stream with an `Err` item, since chunks sent before it
/// are incomplete.
pub async fn get_archive<S>(
    stream: S,
    path: String,
    out: mpsc::Sender<std::io::Result<bytes::Bytes>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Err(e) = relay_archive(stream, path, &out).await {
        let _ = out.send(Err(e)).await;
    }
}

async fn relay_archive<S>(
    stream: S,
    path: String,
    out: &mpsc::Sender<std::io::Result<bytes::Bytes>>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let open = Request {
        kind: Some(request::Kind::GetArchive(proto::GetArchive { path })),
    };
    write_frame(&mut writer, &encode(&open)).await?;
    loop {
        let frame = read_frame(&mut reader)
            .await?
            .ok_or_else(|| protocol_error("agent closed the connection mid-archive"))?;
        match Event::decode(&frame[..]).map(|e| e.kind) {
            Ok(Some(event::Kind::ArchiveChunk(c))) => {
                if out.send(Ok(bytes::Bytes::from(c.data))).await.is_err() {
                    return Ok(()); // consumer gone
                }
            }
            Ok(Some(event::Kind::ArchiveDone(_))) => return Ok(()),
            Ok(Some(event::Kind::ArchiveError(e))) => return Err(agent_failure(e.message)),
            _ => {
                return Err(protocol_error(
                    "agent answered a GetArchive with another event",
                ));
            }
        }
    }
}

/// Ask the agent for the guest's process table, as `ps` with `ps_args`.
///
/// # Errors
///
/// Returns `InvalidArgument` when the agent refuses `ps_args`, and an
/// error if the connection fails or the agent answers with anything else.
pub async fn top<S>(stream: S, ps_args: &[String]) -> super::Result<super::ProcessTable>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let open = Request {
        kind: Some(request::Kind::Top(proto::Top {
            ps_args: ps_args.to_vec(),
        })),
    };
    write_frame(&mut writer, &encode(&open)).await?;
    let frame = read_frame(&mut reader)
        .await?
        .ok_or_else(|| protocol_error("agent closed the connection before answering"))?;
    match Event::decode(&frame[..]).map(|e| e.kind) {
        Ok(Some(event::Kind::Processes(list))) => Ok(super::ProcessTable {
            titles: list.titles,
            rows: list.rows.into_iter().map(|row| row.fields).collect(),
        }),
        Ok(Some(event::Kind::TopError(refusal))) => {
            Err(super::BackendError::InvalidArgument(refusal.message))
        }
        _ => Err(protocol_error("agent answered a Top with another event").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ExecMode, ProcessControl};
    use crate::protocol::RawStream;
    use proto::{Data, Event, Exited, Line, Request, event};
    use tokio::io::{AsyncWriteExt, split};

    /// Minimal in-test agent: reads the opening Exec, then plays a scripted
    /// set of events, optionally echoing the first stdin chunk it receives.
    async fn fake_agent<S>(stream: S, script: Vec<Event>, echo_stdin: bool)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut r, mut w) = split(stream);

        let frame = read_frame(&mut r).await.unwrap().expect("exec frame");
        let req = Request::decode(&frame[..]).unwrap();
        assert!(
            matches!(req.kind, Some(request::Kind::Exec(_))),
            "first frame must be Exec"
        );

        if echo_stdin
            && let Some(frame) = read_frame(&mut r).await.unwrap()
            && let Some(request::Kind::Stdin(s)) = Request::decode(&frame[..]).unwrap().kind
        {
            let text = String::from_utf8_lossy(&s.data).trim().to_string();
            let ev = Event {
                kind: Some(event::Kind::Stdout(Line { text })),
            };
            write_frame(&mut w, &encode(&ev)).await.unwrap();
        }

        for ev in script {
            write_frame(&mut w, &encode(&ev)).await.unwrap();
        }
        let _ = w.shutdown().await;
    }

    fn stdout(text: &str) -> Event {
        Event {
            kind: Some(event::Kind::Stdout(Line {
                text: text.to_string(),
            })),
        }
    }

    fn exited(code: i32) -> Event {
        Event {
            kind: Some(event::Kind::Exited(Exited {
                code,
                ..Default::default()
            })),
        }
    }

    async fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    // ----- SEC-010 frame invariants ----------------------------------------

    #[tokio::test]
    async fn given_zero_length_header_when_read_frame_then_invalid_data() {
        // Arrange
        let (mut client, mut server) = tokio::io::duplex(8);
        client.write_all(&0u32.to_be_bytes()).await.unwrap();
        client.shutdown().await.unwrap();

        // Act
        let err = read_frame(&mut server)
            .await
            .expect_err("len=0 must be rejected");

        // Assert: same contract as the agent side; symmetric kind enables
        // shared log filters.
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("non-zero"));
    }

    #[tokio::test]
    async fn given_over_cap_length_header_when_read_frame_then_invalid_data() {
        // Arrange
        let (mut client, mut server) = tokio::io::duplex(8);
        let oversized = MAX_FRAME_BYTES + 1;
        client.write_all(&oversized.to_be_bytes()).await.unwrap();
        client.shutdown().await.unwrap();

        // Act
        let err = read_frame(&mut server)
            .await
            .expect_err("over-cap must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn given_empty_payload_when_write_frame_then_invalid_input() {
        // Arrange
        let mut sink: Vec<u8> = Vec::new();

        // Act
        let err = write_frame(&mut sink, &[])
            .await
            .expect_err("empty payload must be rejected");

        // Assert
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn given_over_cap_payload_when_write_frame_then_invalid_input() {
        // Arrange
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
    async fn given_agent_stdout_then_exit_when_drive_then_maps_to_stream_events() {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent, vec![stdout("hi"), exited(0)], false));

        let (handle, _kill) = drive_exec(
            daemon,
            "pid-1".to_string(),
            "sb-1".to_string(),
            vec!["echo".to_string(), "hi".to_string()],
            None,
            HashMap::new(),
            ExecMode::default(),
        )
        .await
        .expect("drive_exec");

        let events = collect(handle.output_rx.unwrap()).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, StreamEventKind::Stdout);
        assert_eq!(events[0].line, "hi");
        assert_eq!(events[1].kind, StreamEventKind::Exit);
        assert_eq!(events[1].exit_code, Some(0));
    }

    #[tokio::test]
    async fn given_nonzero_exit_when_drive_then_exit_code_propagates() {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent, vec![exited(42)], false));

        let (handle, _kill) = drive_exec(
            daemon,
            "pid-2".to_string(),
            "sb-1".to_string(),
            vec!["false".to_string()],
            None,
            HashMap::new(),
            ExecMode::default(),
        )
        .await
        .expect("drive_exec");

        let events = collect(handle.output_rx.unwrap()).await;
        assert_eq!(events.last().unwrap().exit_code, Some(42));
    }

    #[tokio::test]
    async fn given_stdin_when_drive_then_forwarded_to_agent() {
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent, vec![exited(0)], true));

        let (handle, _kill) = drive_exec(
            daemon,
            "pid-3".to_string(),
            "sb-1".to_string(),
            vec!["cat".to_string()],
            None,
            HashMap::new(),
            ExecMode::default(),
        )
        .await
        .expect("drive_exec");

        handle
            .stdin_tx
            .as_ref()
            .unwrap()
            .send(bytes::Bytes::from_static(b"ping\n"))
            .await
            .unwrap();

        let events = collect(handle.output_rx.unwrap()).await;
        let stdout_line = events
            .iter()
            .find(|e| e.kind == StreamEventKind::Stdout)
            .expect("stdout event");
        assert_eq!(stdout_line.line, "ping");
    }

    fn data(stream: proto::Stream, bytes: &[u8]) -> Event {
        Event {
            kind: Some(event::Kind::Data(Data {
                stream: stream as i32,
                data: bytes.to_vec(),
            })),
        }
    }

    /// Fake agent that plays `script`, then records request frames until
    /// `limit` arrive or the host closes its side.
    async fn recording_agent<S>(stream: S, script: Vec<Event>, limit: usize) -> Vec<Request>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut r, mut w) = split(stream);
        for ev in script {
            write_frame(&mut w, &encode(&ev)).await.unwrap();
        }
        let mut seen = Vec::new();
        while seen.len() < limit
            && let Ok(Some(frame)) = read_frame(&mut r).await
        {
            seen.push(Request::decode(&frame[..]).unwrap());
        }
        seen
    }

    async fn drive_with(
        daemon: tokio::io::DuplexStream,
        mode: ExecMode,
    ) -> (ProcessHandle, mpsc::Sender<()>) {
        drive_exec(
            daemon,
            "pid-x".to_string(),
            "sb-1".to_string(),
            vec!["sh".to_string()],
            None,
            HashMap::new(),
            mode,
        )
        .await
        .expect("drive_exec")
    }

    #[tokio::test]
    async fn given_data_events_when_drive_then_raw_events_keep_bytes_and_stream() {
        // Arrange
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let payload: &[u8] = b"a\0b\xff";
        tokio::spawn(fake_agent(
            agent,
            vec![
                data(proto::Stream::Stdout, payload),
                data(proto::Stream::Stderr, b"err"),
                exited(0),
            ],
            false,
        ));

        // Act
        let (handle, _kill) = drive_with(
            daemon,
            ExecMode {
                tty: false,
                raw: true,
            },
        )
        .await;
        let events = collect(handle.output_rx.unwrap()).await;

        // Assert
        assert_eq!(
            events[0].kind,
            StreamEventKind::Data {
                stream: RawStream::Stdout,
                bytes: payload.to_vec()
            }
        );
        assert_eq!(
            events[1].kind,
            StreamEventKind::Data {
                stream: RawStream::Stderr,
                bytes: b"err".to_vec()
            }
        );
        assert_eq!(events[2].kind, StreamEventKind::Exit);
    }

    #[tokio::test]
    async fn given_old_line_events_when_drive_then_still_decoded() {
        // Arrange
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent, vec![stdout("old"), exited(0)], false));

        // Act
        let (handle, _kill) = drive_with(daemon, ExecMode::default()).await;
        let events = collect(handle.output_rx.unwrap()).await;

        // Assert
        assert_eq!(events[0].kind, StreamEventKind::Stdout);
        assert_eq!(events[0].line, "old");
    }

    const TEST_ROWS: u32 = 40;
    const TEST_COLS: u32 = 80;

    #[rstest::rstest]
    #[case::old_agent_failure(3, 0, None)]
    #[case::old_agent_signal_death(-1, 0, None)]
    #[case::clean_exit(0, 0, Some(0))]
    #[case::raw_status_present(-1, 9, Some(9))]
    #[tokio::test]
    async fn given_exited_event_when_drive_then_wait_status_maps_per_agent_version(
        #[case] code: i32,
        #[case] wait_status: i32,
        #[case] expected: Option<i32>,
    ) {
        // Arrange
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let ev = Event {
            kind: Some(event::Kind::Exited(Exited { code, wait_status })),
        };
        tokio::spawn(fake_agent(agent, vec![ev], false));

        // Act
        let (handle, _kill) = drive_with(daemon, ExecMode::default()).await;
        let events = collect(handle.output_rx.unwrap()).await;

        // Assert
        assert_eq!(events[0].exit_code, Some(code));
        assert_eq!(events[0].wait_status, expected);
    }

    #[tokio::test]
    async fn given_tty_and_raw_modes_when_drive_then_exec_frame_carries_flags() {
        for (mode, tty, raw) in [
            (
                ExecMode {
                    tty: true,
                    raw: false,
                },
                true,
                false,
            ),
            (
                ExecMode {
                    tty: false,
                    raw: true,
                },
                false,
                true,
            ),
            (ExecMode::default(), false, false),
        ] {
            // Arrange
            let (daemon, agent) = tokio::io::duplex(64 * 1024);
            let peer = tokio::spawn(recording_agent(agent, vec![], 1));

            // Act
            let (handle, kill) = drive_with(daemon, mode).await;
            drop(handle);
            drop(kill);
            let seen = peer.await.unwrap();

            // Assert
            let Some(request::Kind::Exec(exec)) = seen[0].kind.clone() else {
                panic!("first frame must be Exec");
            };
            assert_eq!((exec.tty, exec.raw), (tty, raw));
        }
    }

    #[tokio::test]
    async fn given_control_channel_when_resize_and_signal_sent_then_agent_reads_frames() {
        // Arrange
        const SIGTERM: i32 = 15;
        const EXEC_PLUS_CONTROL_FRAMES: usize = 3;
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let peer = tokio::spawn(recording_agent(agent, vec![], EXEC_PLUS_CONTROL_FRAMES));
        let (handle, _kill) = drive_with(
            daemon,
            ExecMode {
                tty: true,
                raw: false,
            },
        )
        .await;
        let control = handle.control_tx.clone().expect("control channel");

        // Act
        control
            .send(ProcessControl::Resize {
                rows: TEST_ROWS,
                cols: TEST_COLS,
            })
            .await
            .unwrap();
        control.send(ProcessControl::Signal(SIGTERM)).await.unwrap();
        let seen = peer.await.unwrap();

        // Assert
        let kinds: Vec<_> = seen.into_iter().filter_map(|r| r.kind).collect();
        assert!(kinds.contains(&request::Kind::Resize(proto::Resize {
            rows: TEST_ROWS,
            cols: TEST_COLS
        })));
        assert!(kinds.contains(&request::Kind::Signal(proto::Signal { signo: SIGTERM })));
    }

    #[tokio::test]
    async fn given_empty_kill_when_sent_then_agent_reads_kill_frame() {
        // Arrange
        let (daemon, agent) = tokio::io::duplex(64 * 1024);
        let peer = tokio::spawn(recording_agent(agent, vec![], 2));
        let (_handle, kill) = drive_with(daemon, ExecMode::default()).await;

        // Act
        kill.send(()).await.unwrap();
        let seen = peer.await.unwrap();

        // Assert
        assert!(
            seen.iter()
                .any(|r| matches!(r.kind, Some(request::Kind::Kill(_))))
        );
    }

    #[cfg(unix)]
    mod against_real_agent {
        use super::*;

        const SIGTERM: i32 = 15;
        const WAIT_STATUS_SIGTERM: i32 = 0x000F;
        const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

        /// Connect `drive_exec` to the real agent handler over a duplex stream.
        async fn drive_real(command: &[&str], mode: ExecMode) -> (ProcessHandle, mpsc::Sender<()>) {
            let (host, agent) = tokio::io::duplex(64 * 1024);
            tokio::spawn(ward_agent::handle_connection(agent));
            let driven = drive_exec(
                host,
                "pid-real".to_string(),
                "sb-1".to_string(),
                command.iter().map(|s| (*s).to_string()).collect(),
                None,
                HashMap::new(),
                mode,
            )
            .await
            .expect("drive_exec");
            // The caller keeps the kill sender: dropping it ends the writer.
            driven
        }

        #[tokio::test]
        async fn given_signal_sent_when_child_runs_then_it_ends_with_sigterm_status() {
            // Arrange
            let mode = ExecMode {
                tty: false,
                raw: true,
            };
            let (mut handle, _kill) = drive_real(&["sleep", "30"], mode).await;
            let control = handle.control_tx.clone().expect("control channel");

            // Act
            control.send(ProcessControl::Signal(SIGTERM)).await.unwrap();
            let events =
                tokio::time::timeout(TEST_TIMEOUT, collect(handle.output_rx.take().unwrap()))
                    .await
                    .expect("child ignored the signal");

            // Assert
            let exit = events.last().expect("exit event");
            assert_eq!(exit.kind, StreamEventKind::Exit);
            assert_eq!(exit.wait_status, Some(WAIT_STATUS_SIGTERM));
        }

        #[tokio::test]
        async fn given_resize_sent_when_tty_child_runs_stty_then_it_prints_the_new_size() {
            // Arrange: poll until the resize lands, so the test does not
            // depend on the order of the stdin and control channels.
            let size = format!("{TEST_ROWS} {TEST_COLS}");
            let script =
                format!("until [ \"$(stty size)\" = \"{size}\" ]; do sleep 0.05; done; stty size");
            let mode = ExecMode {
                tty: true,
                raw: false,
            };
            let (mut handle, _kill) = drive_real(&["sh", "-c", &script], mode).await;
            let control = handle.control_tx.clone().expect("control channel");

            // Act
            control
                .send(ProcessControl::Resize {
                    rows: TEST_ROWS,
                    cols: TEST_COLS,
                })
                .await
                .unwrap();
            let events =
                tokio::time::timeout(TEST_TIMEOUT, collect(handle.output_rx.take().unwrap()))
                    .await
                    .expect("resize never reached the pty");

            // Assert
            let output: Vec<u8> = events
                .into_iter()
                .filter_map(|e| match e.kind {
                    StreamEventKind::Data { bytes, .. } => Some(bytes),
                    _ => None,
                })
                .flatten()
                .collect();
            assert!(
                String::from_utf8_lossy(&output).contains(&size),
                "got {output:?}"
            );
        }
    }

    const TOP_ARGS: [&str; 2] = ["-o", "pid"];
    const REFUSAL: &str = "unsupported ps option: -x";

    fn top_args() -> Vec<String> {
        TOP_ARGS.map(String::from).to_vec()
    }

    /// Fake guest for `top`: reads the opening frame, then sends `reply`.
    async fn top_guest<S>(stream: S, reply: Option<Event>)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut r, mut w) = split(stream);
        let frame = read_frame(&mut r).await.unwrap().expect("top frame");
        let req = Request::decode(&frame[..]).unwrap();
        let Some(request::Kind::Top(top)) = req.kind else {
            panic!("expected a Top frame");
        };
        assert_eq!(top.ps_args, TOP_ARGS);
        if let Some(event) = reply {
            write_frame(&mut w, &encode(&event)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn given_a_guest_process_list_when_top_then_titles_and_rows_come_back() {
        // Arrange
        let (daemon, guest) = tokio::io::duplex(64 * 1024);
        let list = proto::Processes {
            titles: vec!["PID".into(), "CMD".into()],
            rows: vec![proto::ProcessRow {
                fields: vec!["7".into(), "sleep 600".into()],
            }],
        };
        let reply = Event {
            kind: Some(event::Kind::Processes(list)),
        };
        tokio::spawn(top_guest(guest, Some(reply)));

        // Act
        let table = top(daemon, &top_args()).await.expect("top");

        // Assert
        assert_eq!(table.titles, ["PID", "CMD"]);
        assert_eq!(table.rows, [["7", "sleep 600"]]);
    }

    #[rstest::rstest]
    #[case::other_event(Some(event::Kind::Pong(proto::Pong {})), true)]
    #[case::hang_up(None, true)]
    #[case::refusal(
        Some(event::Kind::TopError(proto::TopError { message: REFUSAL.into() })),
        false
    )]
    #[tokio::test]
    async fn given_a_guest_that_does_not_list_when_top_then_error(
        #[case] reply: Option<event::Kind>,
        #[case] protocol_failure: bool,
    ) {
        // Arrange
        let (daemon, guest) = tokio::io::duplex(64 * 1024);
        tokio::spawn(top_guest(
            guest,
            reply.map(|kind| Event { kind: Some(kind) }),
        ));

        // Act
        let err = top(daemon, &top_args()).await.expect_err("no list");

        // Assert
        use crate::backend::BackendError;
        match err {
            BackendError::Io(e) if protocol_failure => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
            }
            BackendError::InvalidArgument(message) if !protocol_failure => {
                assert_eq!(message, REFUSAL);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
