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

use proto::{Event, Exec, Exited, Line, Pong, Request, event, request};

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
        kind: Some(event::Kind::Exited(Exited {
            code,
            ..Default::default()
        })),
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
        let old = [stdout_event("out".into()), stderr_event("err".into())];
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
        let clean = exited_event(0);
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
