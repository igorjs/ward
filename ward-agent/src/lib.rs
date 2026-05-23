// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Ward guest agent library.
//!
//! Runs commands inside a microVM and speaks the length-prefixed protobuf
//! protocol defined in `proto/ward_agent.proto` over a byte stream (vsock in
//! production; any `AsyncRead + AsyncWrite` in tests). One connection carries
//! exactly one process: an opening `Exec`, then `Stdin`/`Kill` frames inbound
//! and `Stdout`/`Stderr`/`Exited` frames outbound.

use std::process::Stdio;

use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[allow(clippy::doc_lazy_continuation)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/ward.agent.v1.rs"));
}

use proto::{Event, Exec, Exited, Line, Request, event, request};

/// Frames larger than this are rejected as malformed, protecting the agent
/// from a hostile or buggy peer that advertises a huge length.
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// SIGKILL. Declared inline rather than pulling in the `libc` crate (the
/// same minimalism as `ward-core`'s hand-rolled FFI); the value is 9 on
/// every platform ward targets.
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

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

/// Write one length-prefixed frame and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame exceeds u32"))?;
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

fn stdout_event(text: String) -> Event {
    Event {
        kind: Some(event::Kind::Stdout(Line { text })),
    }
}

fn stderr_event(text: String) -> Event {
    Event {
        kind: Some(event::Kind::Stderr(Line { text })),
    }
}

fn exited_event(code: i32) -> Event {
    Event {
        kind: Some(event::Kind::Exited(Exited { code })),
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
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "first frame must be Exec",
            ));
        }
    };

    run_process(exec, reader, writer).await
}

async fn run_process<R, W>(exec: Exec, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    // One writer task drains events from every producer so frames never
    // interleave on the wire.
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(64);
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
        let _ = event_tx.send(exited_event(-1)).await;
        drop(event_tx);
        let _ = writer_task.await;
        return Ok(());
    }

    let mut cmd = Command::new(&exec.command[0]);
    cmd.args(&exec.command[1..]);
    if !exec.working_dir.is_empty() {
        cmd.current_dir(&exec.working_dir);
    }
    cmd.envs(exec.env.iter());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // 127 is the shell convention for "command not found / not
            // executable"; surface the OS error on stderr too.
            let _ = event_tx
                .send(stderr_event(format!("spawn failed: {e}")))
                .await;
            let _ = event_tx.send(exited_event(127)).await;
            drop(event_tx);
            let _ = writer_task.await;
            return Ok(());
        }
    };

    let pid = child.id();
    let child_stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let out_tx = event_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if out_tx.send(stdout_event(line)).await.is_err() {
                break;
            }
        }
    });

    let err_tx = event_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if err_tx.send(stderr_event(line)).await.is_err() {
                break;
            }
        }
    });

    // Inbound frames: stdin chunks and kill requests. Killing targets the
    // pid directly (not `&mut child`) so it never aliases the `child.wait()`
    // below.
    let stdin_task = tokio::spawn(async move {
        let mut reader = reader;
        let mut child_stdin = child_stdin;
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(frame)) => match decode_request(&frame) {
                    Ok(req) => match req.kind {
                        Some(request::Kind::Stdin(s)) => {
                            if let Some(si) = child_stdin.as_mut() {
                                if s.data.is_empty() {
                                    let _ = si.shutdown().await;
                                    child_stdin = None; // explicit EOF
                                } else if si.write_all(&s.data).await.is_err() {
                                    child_stdin = None;
                                }
                            }
                        }
                        Some(request::Kind::Kill(_)) => signal_kill(pid),
                        _ => {}
                    },
                    Err(_) => break, // malformed frame: stop reading
                },
                // Daemon closed its write half: no more stdin or kill frames
                // are coming. Close the child's stdin so stdin-reading
                // programs see EOF, but let the process run to completion —
                // closing the input channel is not a kill.
                Ok(None) | Err(_) => {
                    if let Some(mut si) = child_stdin.take() {
                        let _ = si.shutdown().await;
                    }
                    break;
                }
            }
        }
    });

    let status = child.wait().await;
    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);

    // Drain output before announcing exit so every line precedes Exited.
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let _ = event_tx.send(exited_event(code)).await;

    drop(event_tx);
    stdin_task.abort();
    let _ = writer_task.await;
    Ok(())
}

#[cfg(unix)]
fn signal_kill(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: kill(2) with a pid we own and a constant signal number is
        // sound; a stale pid simply returns ESRCH, which we ignore.
        unsafe {
            kill(pid as i32, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn signal_kill(_pid: Option<u32>) {}

#[cfg(test)]
mod tests {
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
                None => {}
            }
        }
        (out, err, code)
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
