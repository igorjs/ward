// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Attach: one bidirectional stream onto a sandbox's main process or onto a
//! command started for the stream. A client disconnect only detaches.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tracing::{info, warn};

use super::server::api_err_to_status;
use crate::backend::ProcessControl;
use crate::pb::attach_message::Kind;
use crate::pb::{
    AttachData, AttachExit, AttachMessage, AttachMode, AttachReplayEnd, AttachStart, AttachStream,
};
use crate::protocol::{OutputChunk, RawStream, StreamEvent};
use crate::sandbox::{
    ProcessIo, SandboxManager, decode_wait_status, event_chunk, event_wait_status,
};

/// Messages queued towards the client before the pump waits for it.
const OUTBOUND_CAPACITY: usize = 16;
const SIGKILL: i32 = 9;
const NO_EXIT_STATUS: &str = "process ended without an exit status";

type Outbound = mpsc::Sender<Result<AttachMessage, Status>>;

/// Start serving an authorized attach and return the messages for the client.
pub(crate) async fn serve<S>(
    manager: Arc<SandboxManager>,
    id: String,
    start: AttachStart,
    inbound: S,
) -> Result<mpsc::Receiver<Result<AttachMessage, Status>>, Status>
where
    S: Stream<Item = Result<AttachMessage, Status>> + Send + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let read_only = start.replay;
    match AttachMode::try_from(start.mode).unwrap_or(AttachMode::Unspecified) {
        AttachMode::MainProcess => {
            let mut io = manager.main_io(&id).await.map_err(api_err_to_status)?;
            if read_only {
                // A watcher's stdin clone would keep the writer's EOF from landing.
                io = ProcessIo {
                    stdin: None,
                    control: None,
                };
            }
            info!(sandbox_id = %id, read_only, "attach to main process");
            let buffer_only = start.replay && !start.follow;
            let input = forward_input(
                Arc::clone(&manager),
                id.clone(),
                true,
                read_only,
                inbound,
                io,
            );
            run(pump_main(manager, id, buffer_only, tx), input);
        }
        AttachMode::Exec if !start.replay => {
            let workdir = (!start.workdir.is_empty()).then_some(start.workdir);
            let handle = manager
                .spawn_attached(
                    &id,
                    start.command,
                    workdir,
                    start.env,
                    start.tty,
                    &start.user,
                )
                .await
                .map_err(api_err_to_status)?;
            info!(sandbox_id = %id, pid = %handle.pid, "attach to new exec");
            let io = ProcessIo {
                stdin: handle.stdin_tx,
                control: handle.control_tx,
            };
            let reaper = io.control.clone();
            let output = handle
                .output_rx
                .ok_or_else(|| Status::internal("internal error"))?;
            run(
                pump_exec(output, reaper, tx),
                forward_input(manager, id, false, false, inbound, io),
            );
        }
        AttachMode::Exec => {
            return Err(Status::invalid_argument(
                "replay applies to the main process only",
            ));
        }
        AttachMode::Unspecified => {
            return Err(Status::invalid_argument(
                "attach mode must be main process or exec",
            ));
        }
    }
    Ok(rx)
}

/// Run the input task for as long as the output pump lives.
fn run(
    pump: impl Future<Output = ()> + Send + 'static,
    input: impl Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        let input = tokio::spawn(input);
        pump.await;
        input.abort();
        info!("attach ended");
    });
}

async fn send_data(tx: &Outbound, chunk: OutputChunk) -> bool {
    if chunk.bytes.is_empty() {
        return true;
    }
    let stream = match chunk.stream {
        RawStream::Stdout => AttachStream::Stdout,
        RawStream::Stderr => AttachStream::Stderr,
    };
    let kind = Kind::Data(AttachData {
        data: chunk.bytes.to_vec(),
        stream: stream.into(),
        timestamp: Some(prost_types::Timestamp {
            seconds: chunk.at.timestamp(),
            nanos: chunk
                .at
                .timestamp_subsec_nanos()
                .try_into()
                .unwrap_or_default(),
        }),
    });
    tx.send(Ok(AttachMessage { kind: Some(kind) }))
        .await
        .is_ok()
}

async fn send_replay_end(tx: &Outbound) -> bool {
    let kind = Kind::ReplayEnd(AttachReplayEnd {});
    tx.send(Ok(AttachMessage { kind: Some(kind) }))
        .await
        .is_ok()
}

async fn send_exit(tx: &Outbound, raw_status: i32) {
    let kind = Kind::Exit(AttachExit {
        exit_code: decode_wait_status(raw_status),
    });
    let _ = tx.send(Ok(AttachMessage { kind: Some(kind) })).await;
}

async fn send_error(tx: &Outbound, status: Status) {
    let _ = tx.send(Err(status)).await;
}

/// Replay the buffered output, then follow live output until the main process
/// ends and report its exit code.
async fn pump_main(manager: Arc<SandboxManager>, id: String, buffer_only: bool, tx: Outbound) {
    let (buffered, mut live) = match manager.subscribe_main_output(&id).await {
        Ok(subscription) => subscription,
        Err(e) => return send_error(&tx, api_err_to_status(e)).await,
    };
    for chunk in buffered {
        if !send_data(&tx, chunk).await {
            return;
        }
    }
    if !send_replay_end(&tx).await || buffer_only {
        return;
    }
    let exit = manager.main_exit_status(&id);
    tokio::pin!(exit);
    loop {
        tokio::select! {
            _ = tx.closed() => return,
            chunk = live.recv() => match chunk {
                Ok(chunk) => {
                    if !send_data(&tx, chunk).await {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(sandbox_id = %id, missed, "attach reader lagged behind the output");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            status = &mut exit => {
                // The pump stores all output before it publishes the status.
                loop {
                    match live.try_recv() {
                        Ok(chunk) => {
                            if !send_data(&tx, chunk).await {
                                return;
                            }
                        }
                        Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                            warn!(sandbox_id = %id, missed, "attach reader lagged behind the output");
                        }
                        Err(_) => break,
                    }
                }
                match status {
                    Ok(raw) => send_exit(&tx, raw).await,
                    Err(e) => send_error(&tx, api_err_to_status(e)).await,
                }
                return;
            }
        }
    }
}

/// Forward a new exec's output, then its exit code. Kills the process if the
/// client disconnects first.
async fn pump_exec(
    mut output: mpsc::Receiver<StreamEvent>,
    control: Option<mpsc::Sender<ProcessControl>>,
    tx: Outbound,
) {
    loop {
        let event = tokio::select! {
            _ = tx.closed() => {
                // Nobody owns the process once its client is gone.
                if let Some(control) = control {
                    let _ = control.send(ProcessControl::Signal(SIGKILL)).await;
                }
                return;
            }
            event = output.recv() => event,
        };
        let Some(event) = event else {
            return send_error(&tx, Status::aborted(NO_EXIT_STATUS)).await;
        };
        if let Some(chunk) = event_chunk(&event) {
            if !send_data(&tx, chunk).await {
                return;
            }
        } else {
            match event_wait_status(&event) {
                Some(raw) => send_exit(&tx, raw).await,
                None => send_error(&tx, Status::aborted(NO_EXIT_STATUS)).await,
            }
            return;
        }
    }
}

/// Apply the client's stdin, resize and signal messages to the process.
/// A read-only client is heard but ignored.
async fn forward_input<S>(
    manager: Arc<SandboxManager>,
    id: String,
    is_main: bool,
    read_only: bool,
    mut inbound: S,
    mut io: ProcessIo,
) where
    S: Stream<Item = Result<AttachMessage, Status>> + Unpin,
{
    while let Some(Ok(message)) = inbound.next().await {
        if read_only {
            continue;
        }
        match message.kind {
            Some(Kind::Stdin(stdin)) if stdin.data.is_empty() => {
                io.stdin = None;
                if is_main {
                    manager.close_main_stdin(&id).await;
                }
            }
            Some(Kind::Stdin(stdin)) => {
                if let Some(tx) = &io.stdin {
                    let _ = tx.send(Bytes::from(stdin.data)).await;
                }
            }
            Some(Kind::Resize(resize)) => {
                let control = ProcessControl::Resize {
                    rows: resize.rows,
                    cols: resize.cols,
                };
                send_control(&io, control).await;
            }
            Some(Kind::Signal(signal)) => {
                send_control(&io, ProcessControl::Signal(signal.signo)).await
            }
            _ => warn!(sandbox_id = %id, "attach ignored an unexpected message"),
        }
    }
}

async fn send_control(io: &ProcessIo, control: ProcessControl) {
    if let Some(tx) = &io.control {
        let _ = tx.send(control).await;
    }
}

#[cfg(all(test, not(feature = "krunvm")))]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::pb::{AttachReplayEnd as ReplayEnd, AttachResize, AttachSignal, AttachStdin};
    use crate::sandbox::{create_with_command, host_manager};

    const TEST_DEADLINE: Duration = Duration::from_secs(30);
    const DETACH_DEADLINE: Duration = Duration::from_secs(5);
    const POLL_STEP: Duration = Duration::from_millis(100);
    const QUIET_WINDOW: Duration = Duration::from_millis(300);
    const OUTPUT_CAP: usize = 1024 * 1024;
    const SIGTERM: i32 = 15;
    const FEED_CAPACITY: usize = 16;

    type Feed = mpsc::Sender<Result<AttachMessage, Status>>;
    type Replies = mpsc::Receiver<Result<AttachMessage, Status>>;

    fn message(kind: Kind) -> Result<AttachMessage, Status> {
        Ok(AttachMessage { kind: Some(kind) })
    }

    fn stdin(data: &[u8]) -> Result<AttachMessage, Status> {
        message(Kind::Stdin(AttachStdin {
            data: data.to_vec(),
        }))
    }

    fn main_start(id: &str) -> AttachStart {
        AttachStart {
            sandbox_id: id.into(),
            mode: AttachMode::MainProcess as i32,
            ..Default::default()
        }
    }

    async fn attach(mgr: &Arc<SandboxManager>, id: &str, start: AttachStart) -> (Feed, Replies) {
        let (feed, inbound) = mpsc::channel(FEED_CAPACITY);
        let replies = serve(
            Arc::clone(mgr),
            id.into(),
            start,
            ReceiverStream::new(inbound),
        )
        .await
        .expect("serve");
        (feed, replies)
    }

    /// Read replies until the exit code arrives or the stream ends.
    async fn collect(replies: &mut Replies) -> (String, Option<i32>) {
        let (out, err, code) = collect_streams(replies).await;
        (out + &err, code)
    }

    /// Read replies to the end, keeping stdout and stderr apart.
    async fn collect_streams(replies: &mut Replies) -> (String, String, Option<i32>) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let read = async {
            while let Some(reply) = replies.recv().await {
                match reply.expect("reply").kind {
                    Some(Kind::Data(data)) if data.stream == AttachStream::Stderr as i32 => {
                        err.extend(data.data)
                    }
                    Some(Kind::Data(data)) => out.extend(data.data),
                    Some(Kind::Exit(exit)) => return Some(exit.exit_code),
                    _ => {}
                }
            }
            None
        };
        let code = tokio::time::timeout(TEST_DEADLINE, read)
            .await
            .expect("stream ended in time");
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
            code,
        )
    }

    /// Read replies until the text holds `expected`.
    async fn read_until(replies: &mut Replies, expected: &str) -> String {
        let mut text = String::new();
        let read = async {
            while !text.contains(expected) {
                let reply = replies.recv().await.expect("stream open").expect("reply");
                if let Some(Kind::Data(data)) = reply.kind {
                    text.push_str(&String::from_utf8_lossy(&data.data));
                }
            }
        };
        tokio::time::timeout(TEST_DEADLINE, read)
            .await
            .expect("output in time");
        text
    }

    #[rstest::rstest]
    #[case::exit_0(0x0000, 0)]
    #[case::exit_1(0x0100, 1)]
    #[case::exit_127(0x7f00, 127)]
    #[case::exit_255(0xff00, 255)]
    #[case::signal_1(0x0001, 129)]
    #[case::signal_2(0x0002, 130)]
    #[case::signal_9(0x0009, 137)]
    #[case::signal_15(0x000f, 143)]
    fn given_raw_wait_status_when_decoded_then_shell_exit_code(
        #[case] raw: i32,
        #[case] code: i32,
    ) {
        // Arrange / Act
        let decoded = decode_wait_status(raw);

        // Assert
        assert_eq!(decoded, code);
    }

    #[tokio::test]
    async fn given_cat_main_process_when_bytes_then_eof_sent_then_it_echoes_and_exits_0() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["cat"], false).await;
        let (feed, mut replies) = attach(&mgr, &id, main_start(&id)).await;

        // Act
        feed.send(stdin(b"hello\n")).await.unwrap();
        feed.send(stdin(b"")).await.unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await, ("hello\n".into(), Some(0)));
    }

    #[tokio::test]
    async fn given_cat_exec_when_bytes_then_eof_sent_then_it_echoes_and_exits_0() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        let start = AttachStart {
            sandbox_id: id.clone(),
            mode: AttachMode::Exec as i32,
            command: vec!["cat".into()],
            ..Default::default()
        };
        let (feed, mut replies) = attach(&mgr, &id, start).await;

        // Act
        feed.send(stdin(b"hello\n")).await.unwrap();
        feed.send(stdin(b"")).await.unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await, ("hello\n".into(), Some(0)));
    }

    #[tokio::test]
    async fn given_replay_watcher_when_writer_sends_eof_then_cat_still_exits_0() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["cat"], false).await;
        let watcher_start = AttachStart {
            replay: true,
            follow: true,
            ..main_start(&id)
        };
        let (_watcher_feed, _watcher_replies) = attach(&mgr, &id, watcher_start).await;
        let (feed, mut replies) = attach(&mgr, &id, main_start(&id)).await;

        // Act
        feed.send(stdin(b"hi\n")).await.unwrap();
        feed.send(stdin(b"")).await.unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await, ("hi\n".into(), Some(0)));
    }

    #[tokio::test]
    async fn given_exec_client_that_disconnects_when_polled_then_the_process_is_killed() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > {}; echo up; sleep 1000", pid_file.display());
        let start = AttachStart {
            sandbox_id: id.clone(),
            mode: AttachMode::Exec as i32,
            command: vec!["sh".into(), "-c".into(), script],
            ..Default::default()
        };
        let (feed, mut replies) = attach(&mgr, &id, start).await;
        read_until(&mut replies, "up").await;
        let pid = std::fs::read_to_string(&pid_file)
            .expect("pid")
            .trim()
            .to_string();

        // Act
        drop(feed);
        drop(replies);

        // Assert
        let gone = async {
            while std::process::Command::new("kill")
                .args(["-0", &pid])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                tokio::time::sleep(POLL_STEP).await;
            }
        };
        tokio::time::timeout(DETACH_DEADLINE, gone)
            .await
            .expect("exec process killed after the client left");
    }

    #[tokio::test]
    async fn given_main_process_exiting_7_when_go_sent_then_stream_reports_7() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x; exit 7"], false).await;
        let (feed, mut replies) = attach(&mgr, &id, main_start(&id)).await;

        // Act
        feed.send(stdin(b"go\n")).await.unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await.1, Some(7));
    }

    #[tokio::test]
    async fn given_process_without_pty_when_sigterm_sent_then_stream_reports_143() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sleep", "60"], false).await;
        let (feed, mut replies) = attach(&mgr, &id, main_start(&id)).await;

        // Act
        feed.send(message(Kind::Signal(AttachSignal { signo: SIGTERM })))
            .await
            .unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await.1, Some(143));
    }

    #[tokio::test]
    async fn given_main_process_killed_with_sigkill_when_attached_then_stream_reports_137() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        let (_feed, mut replies) = attach(&mgr, &id, main_start(&id)).await;
        let pid: i32 = mgr.main_pid(&id).await.unwrap().parse().unwrap();

        // Act
        let pid = rustix::process::Pid::from_raw(pid).expect("pid");
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("kill");

        // Assert
        assert_eq!(collect(&mut replies).await.1, Some(137));
    }

    #[tokio::test]
    async fn given_tty_stream_when_resized_to_40_by_80_then_stty_size_prints_it() {
        // Arrange: the script waits until the pty reports the new size.
        let mgr = host_manager(OUTPUT_CAP);
        let script = "until [ \"$(stty size)\" = \"40 80\" ]; do sleep 0.05; done; echo resized";
        let id = create_with_command(&mgr, &["sh", "-c", script], true).await;
        let start = AttachStart {
            tty: true,
            ..main_start(&id)
        };
        let (feed, mut replies) = attach(&mgr, &id, start).await;

        // Act
        let resize = Kind::Resize(AttachResize { rows: 40, cols: 80 });
        feed.send(message(resize)).await.unwrap();

        // Assert
        assert_eq!(collect(&mut replies).await, ("resized\r\n".into(), Some(0)));
    }

    #[tokio::test]
    async fn given_earlier_output_when_replayed_with_follow_then_lines_arrive_in_order_and_stdin_is_ignored()
     {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let gate_dir = tempfile::tempdir().expect("tempdir");
        let gate = gate_dir.path().join("go");
        let script = format!(
            "echo one; until [ -e {} ]; do sleep 0.05; done; echo two; exec cat",
            gate.display()
        );
        let id = create_with_command(&mgr, &["sh", "-c", &script], false).await;
        let start = AttachStart {
            replay: true,
            follow: true,
            ..main_start(&id)
        };
        let (feed, mut replies) = attach(&mgr, &id, start).await;
        read_until(&mut replies, "one\n").await;

        // Act
        feed.send(stdin(b"ignored\n")).await.unwrap();
        std::fs::write(&gate, b"").expect("open the gate");
        let live = read_until(&mut replies, "two\n").await;
        let echoed = tokio::time::timeout(QUIET_WINDOW, replies.recv()).await;

        // Assert
        assert_eq!(live, "two\n");
        assert!(echoed.is_err(), "cat received the ignored stdin");
    }

    async fn wait_for_buffered_output(mgr: &SandboxManager, id: &str) {
        let buffered = async {
            loop {
                let (buffered, _) = mgr.subscribe_main_output(id).await.expect("subscribe");
                if !buffered.is_empty() {
                    break;
                }
                tokio::time::sleep(POLL_STEP).await;
            }
        };
        tokio::time::timeout(TEST_DEADLINE, buffered)
            .await
            .expect("main process output was never buffered");
    }

    #[tokio::test]
    async fn given_replay_without_follow_when_attached_then_buffer_arrives_and_stream_ends() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo one; exec cat"], false).await;
        wait_for_buffered_output(&mgr, &id).await;
        let start = AttachStart {
            replay: true,
            ..main_start(&id)
        };
        let (_feed, mut replies) = attach(&mgr, &id, start).await;

        // Act
        let (text, code) = collect(&mut replies).await;

        // Assert: the stream ended with the buffer and no exit code.
        assert_eq!((text.as_str(), code), ("one\n", None));
    }

    #[tokio::test]
    async fn given_buffered_output_when_replayed_with_follow_then_the_marker_follows_the_buffer_and_data_is_stamped()
     {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo one; exec cat"], false).await;
        wait_for_buffered_output(&mgr, &id).await;
        let start = AttachStart {
            replay: true,
            follow: true,
            ..main_start(&id)
        };

        // Act
        let (_feed, mut replies) = attach(&mgr, &id, start).await;
        let first = replies.recv().await.expect("open").expect("reply");
        let second = replies.recv().await.expect("open").expect("reply");

        // Assert
        let Some(Kind::Data(data)) = first.kind else {
            panic!("expected data first, got {first:?}");
        };
        assert_eq!(data.data, b"one\n");
        assert!(data.timestamp.is_some_and(|t| t.seconds > 0));
        assert_eq!(second.kind, Some(Kind::ReplayEnd(ReplayEnd {})));
    }

    #[tokio::test]
    async fn given_exec_writing_to_both_streams_when_attached_then_they_stay_apart() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["cat"], false).await;
        let start = AttachStart {
            sandbox_id: id.clone(),
            mode: AttachMode::Exec as i32,
            command: vec!["sh".into(), "-c".into(), "echo out; echo err >&2".into()],
            ..Default::default()
        };

        // Act
        let (_feed, mut replies) = attach(&mgr, &id, start).await;

        // Assert
        let (out, err, code) = collect_streams(&mut replies).await;
        assert_eq!(
            (out.as_str(), err.as_str(), code),
            ("out\n", "err\n", Some(0))
        );
    }

    #[tokio::test]
    async fn given_main_process_that_wrote_to_both_streams_when_replayed_then_they_stay_apart() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["sh", "-c", "echo out; echo err >&2"], false).await;
        let exited = mgr.main_exit_status(&id).await;
        assert!(exited.is_ok());
        let start = AttachStart {
            replay: true,
            ..main_start(&id)
        };

        // Act
        let (_feed, mut replies) = attach(&mgr, &id, start).await;

        // Assert
        let (out, err, _) = collect_streams(&mut replies).await;
        assert_eq!((out.as_str(), err.as_str()), ("out\n", "err\n"));
    }

    #[tokio::test]
    async fn given_client_disconnect_when_polled_then_main_process_keeps_running() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &["cat"], false).await;
        let (feed, replies) = attach(&mgr, &id, main_start(&id)).await;

        // Act
        drop((feed, replies));

        // Assert
        let deadline = std::time::Instant::now() + DETACH_DEADLINE;
        while std::time::Instant::now() < deadline {
            let ended =
                tokio::time::timeout(Duration::from_millis(1), mgr.main_exit_status(&id)).await;
            assert!(ended.is_err(), "main process ended after detach");
            tokio::time::sleep(POLL_STEP).await;
        }
    }

    #[tokio::test]
    async fn given_sandbox_without_command_when_attached_to_main_then_it_fails() {
        // Arrange
        let mgr = host_manager(OUTPUT_CAP);
        let id = create_with_command(&mgr, &[], false).await;
        let (_feed, inbound) = mpsc::channel(FEED_CAPACITY);

        // Act
        let result = serve(
            mgr,
            id.clone(),
            main_start(&id),
            ReceiverStream::new(inbound),
        )
        .await;

        // Assert
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }
}
