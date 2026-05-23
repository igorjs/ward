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

use super::ProcessHandle;
use crate::protocol::{StreamEvent, StreamEventKind};

/// Agent protocol types, generated from `proto/ward_agent.proto`.
pub mod proto {
    tonic::include_proto!("ward.agent.v1");
}

use proto::{Event, Exec, Request, event, request};

const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame exceeds u32"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

fn encode<M: Message>(m: &M) -> Vec<u8> {
    let mut buf = Vec::with_capacity(m.encoded_len());
    m.encode(&mut buf)
        .expect("encoding into a Vec is infallible");
    buf
}

fn now() -> std::time::SystemTime {
    std::time::SystemTime::now()
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
        })),
    };
    write_frame(&mut writer, &encode(&exec)).await?;

    let (output_tx, output_rx) = mpsc::channel::<StreamEvent>(64);
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<bytes::Bytes>(64);
    let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);

    // Outbound: forward stdin chunks and a single kill, framing each.
    tokio::spawn(async move {
        let mut stdin_open = true;
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
                    timestamp: now(),
                    duration_ms: 0,
                },
                Some(event::Kind::Stderr(l)) => StreamEvent {
                    kind: StreamEventKind::Stderr,
                    line: l.text,
                    exit_code: None,
                    timestamp: now(),
                    duration_ms: 0,
                },
                Some(event::Kind::Exited(x)) => {
                    let _ = output_tx
                        .send(StreamEvent {
                            kind: StreamEventKind::Exit,
                            line: String::new(),
                            exit_code: Some(x.code),
                            timestamp: now(),
                            duration_ms: started
                                .elapsed()
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0),
                        })
                        .await;
                    break; // terminal event; closing output_tx ends the stream
                }
                None => continue,
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
    };
    Ok((handle, kill_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::{Event, Exited, Line, Request, event};
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
            kind: Some(event::Kind::Exited(Exited { code })),
        }
    }

    async fn collect(mut rx: mpsc::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
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
}
