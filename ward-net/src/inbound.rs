// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host-to-guest TCP port forwarding.
//!
//! A host `TcpListener` accepts connections and hands each one to the
//! smoltcp task, which opens a TCP connection from the gateway address to
//! the guest and relays bytes both ways. Only TCP, and only toward the
//! guest: egress policy does not apply to these flows.

use std::collections::VecDeque;
use std::hash::{BuildHasher, RandomState};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::Error;
use crate::smoltcp_backend::{HALF_CLOSE_TIMEOUT, StackCommand};

/// Concurrent connections one published port accepts. Further connections
/// are closed at accept time, so a flood cannot grow the flow table.
pub const MAX_INBOUND_PER_LISTENER: usize = 64;

/// Concurrent inbound connections across all ports of one sandbox network.
pub const MAX_INBOUND_TOTAL: usize = 256;

/// How long the guest has to complete the TCP handshake before the host
/// connection is reset.
pub const INBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeouts applied to inbound flows. A field of the stack so tests can
/// shorten them.
#[derive(Debug, Clone, Copy)]
pub struct InboundTuning {
    /// Longest a half-closed flow may sit idle (no bytes either way).
    pub half_close_timeout: Duration,
    /// Longest the guest may take to answer the connection attempt.
    pub connect_timeout: Duration,
}

impl Default for InboundTuning {
    fn default() -> Self {
        InboundTuning {
            half_close_timeout: HALF_CLOSE_TIMEOUT,
            connect_timeout: INBOUND_CONNECT_TIMEOUT,
        }
    }
}

/// Pause after a failed `accept` (for example out of file descriptors) so
/// the accept loop does not spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Bytes moved per read, matching the smoltcp socket buffer size.
const CHUNK_BYTES: usize = crate::smoltcp_backend::TCP_SOCKET_BUFFER_BYTES;

/// Chunks queued in each direction before the sender waits. Small on
/// purpose: backpressure should reach the TCP windows, not grow buffers.
const CHANNEL_CAPACITY: usize = 16;

/// Longest a single write to the host client may take before the flow is
/// reset.
const HOST_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Local ports used for connections toward the guest (IANA dynamic range).
/// The range is far larger than `MAX_INBOUND_TOTAL` plus `RECENT_PORTS`.
const EPHEMERAL_PORT_START: u16 = 49152;
const EPHEMERAL_PORT_COUNT: u32 = u16::MAX as u32 - EPHEMERAL_PORT_START as u32 + 1;

/// Retired local ports held back from reuse, so a late segment of an old
/// connection cannot be mistaken for part of a new one.
const RECENT_PORTS: usize = 64;

/// Guest to host messages. A channel closed without `Fin` means reset.
enum ToHost {
    Data(Vec<u8>),
    Fin,
}

/// Host to guest messages. A channel closed without `Eof` means the host
/// side failed.
enum FromHost {
    Data(Vec<u8>),
    Eof,
}

/// A freshly accepted host connection on its way to the smoltcp task.
pub struct NewInbound {
    stream: TcpStream,
    guest_port: u16,
    alive: Weak<()>,
    _permits: [OwnedSemaphorePermit; 2],
}

impl NewInbound {
    pub(crate) fn guest_port(&self) -> u16 {
        self.guest_port
    }

    /// Refuses the connection: the host client sees a reset, not a close.
    pub(crate) fn reset(self) {
        reset_on_close(&self.stream);
    }
}

/// A published port. Dropping it closes the listener and resets every
/// connection accepted through it.
pub struct InboundHandle {
    local_addr: SocketAddr,
    accept_task: JoinHandle<()>,
    // Flows hold a `Weak` to this; they reset once it is gone.
    _alive: Arc<()>,
}

impl InboundHandle {
    /// Address the host listener is bound to (carries the chosen port when
    /// port 0 was requested).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Closes the listener and returns once the host port is free again,
    /// which a plain drop does not wait for.
    pub async fn close(mut self) {
        self.accept_task.abort();
        let _ = (&mut self.accept_task).await;
    }
}

impl Drop for InboundHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Binds `bind` and starts forwarding accepted connections to `guest_port`
/// through `cmd_tx`.
pub(crate) async fn start_listener(
    bind: SocketAddr,
    guest_port: u16,
    cmd_tx: mpsc::Sender<StackCommand>,
    total: Arc<Semaphore>,
) -> Result<InboundHandle, Error> {
    if !bind.ip().is_loopback() {
        tracing::warn!(address = %bind, "publishing a guest port on a non-loopback address");
    }
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| Error::Listen { addr: bind, source })?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| Error::Process(format!("cannot read listener address: {err}")))?;
    let alive = Arc::new(());
    let per_listener = Arc::new(Semaphore::new(MAX_INBOUND_PER_LISTENER));
    let weak = Arc::downgrade(&alive);
    let accept_task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _peer)) => stream,
                Err(err) => {
                    tracing::warn!(error = %err, "accept failed on a published port");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            };
            let (Ok(a), Ok(b)) = (
                Arc::clone(&per_listener).try_acquire_owned(),
                Arc::clone(&total).try_acquire_owned(),
            ) else {
                tracing::warn!(
                    port = guest_port,
                    "inbound connection limit reached, closing"
                );
                continue;
            };
            let new = NewInbound {
                stream,
                guest_port,
                alive: weak.clone(),
                _permits: [a, b],
            };
            if cmd_tx.send(StackCommand::Inbound(new)).await.is_err() {
                break;
            }
        }
    });
    Ok(InboundHandle {
        local_addr,
        accept_task,
        _alive: alive,
    })
}

/// Spawns the task owning `stream` and returns its channels and handle. The
/// read and write halves run independently so one side can close while the
/// other keeps flowing; if either fails, both are torn down and the
/// connection is reset. Dropping the returned receiver ends the task, which
/// closes the socket. `wake` is signalled whenever the task has work for the
/// stack, so the stack need not poll fast to notice it.
fn spawn_host_io(
    stream: TcpStream,
    wake: Arc<Notify>,
) -> (
    mpsc::Sender<ToHost>,
    mpsc::Receiver<FromHost>,
    JoinHandle<()>,
) {
    let (to_host_tx, mut to_host_rx) = mpsc::channel::<ToHost>(CHANNEL_CAPACITY);
    let (from_host_tx, from_host_rx) = mpsc::channel::<FromHost>(CHANNEL_CAPACITY);
    let (mut read_half, mut write_half) = stream.into_split();
    let write_wake = Arc::clone(&wake);
    let task = tokio::spawn(async move {
        let write_loop = async {
            loop {
                match to_host_rx.recv().await {
                    Some(ToHost::Data(chunk)) => {
                        match tokio::time::timeout(HOST_WRITE_TIMEOUT, write_half.write_all(&chunk))
                            .await
                        {
                            Ok(Ok(())) => write_wake.notify_one(),
                            _ => return Err(()),
                        }
                    }
                    Some(ToHost::Fin) => {
                        let _ = write_half.shutdown().await;
                        return Ok(());
                    }
                    None => return Err(()),
                }
            }
        };
        let read_loop = async {
            let mut buf = vec![0u8; CHUNK_BYTES];
            loop {
                let read = tokio::select! {
                    read = read_half.read(&mut buf) => read,
                    // The flow was retired: stop waiting on an idle client.
                    () = from_host_tx.closed() => return Err(()),
                };
                match read {
                    Ok(0) => {
                        let sent = from_host_tx.send(FromHost::Eof).await.map_err(|_| ());
                        wake.notify_one();
                        return sent;
                    }
                    Ok(n) => {
                        let chunk = FromHost::Data(buf[..n].to_vec());
                        from_host_tx.send(chunk).await.map_err(|_| ())?;
                        wake.notify_one();
                    }
                    Err(_) => return Err(()),
                }
            }
        };
        if tokio::try_join!(write_loop, read_loop).is_err()
            && let Ok(stream) = read_half.reunite(write_half)
        {
            reset_on_close(&stream);
        }
    });
    (to_host_tx, from_host_rx, task)
}

/// Makes closing `stream` send a TCP RST instead of a FIN, so the host
/// client sees the failure rather than a clean end of stream.
fn reset_on_close(stream: &TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: the fd is open for the stream's lifetime and the pointer and
    // length describe a valid `linger` value.
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        tracing::debug!("could not set SO_LINGER; host client sees FIN instead of RST");
    }
}

/// One host connection relayed to the guest.
pub(crate) struct InboundFlow {
    handle: SocketHandle,
    local_port: u16,
    to_host: mpsc::Sender<ToHost>,
    from_host: mpsc::Receiver<FromHost>,
    /// Host chunk not yet fully accepted by the guest socket.
    pending: Option<Vec<u8>>,
    host_eof: bool,
    fin_to_host: bool,
    fin_to_guest: bool,
    started: Instant,
    half_closed: bool,
    /// Last time bytes moved in either direction.
    last_activity: Instant,
    /// Bytes moved since the stack last asked, see `take_moved`.
    moved: bool,
    alive: Weak<()>,
    _permits: [OwnedSemaphorePermit; 2],
}

impl InboundFlow {
    pub(crate) fn new(
        new: NewInbound,
        handle: SocketHandle,
        local_port: u16,
        wake: Arc<Notify>,
    ) -> Self {
        let (to_host, from_host, _task) = spawn_host_io(new.stream, wake);
        InboundFlow {
            handle,
            local_port,
            to_host,
            from_host,
            pending: None,
            host_eof: false,
            fin_to_host: false,
            fin_to_guest: false,
            started: Instant::now(),
            half_closed: false,
            last_activity: Instant::now(),
            moved: false,
            alive: new.alive,
            _permits: new._permits,
        }
    }

    pub(crate) fn handle(&self) -> SocketHandle {
        self.handle
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Whether bytes moved since the last call.
    pub(crate) fn take_moved(&mut self) -> bool {
        std::mem::take(&mut self.moved)
    }

    fn mark_moved(&mut self) {
        self.moved = true;
        self.last_activity = Instant::now();
    }

    /// Moves bytes and close signals between the guest socket and the host
    /// task. Returns true once the flow is finished or must be torn down;
    /// the caller then aborts the socket and drops the flow.
    pub(crate) fn pump(&mut self, socket: &mut tcp::Socket, tuning: &InboundTuning) -> bool {
        if self.alive.strong_count() == 0 {
            return true;
        }
        match socket.state() {
            tcp::State::Closed => return true,
            tcp::State::SynSent => {
                return self.started.elapsed() >= tuning.connect_timeout;
            }
            _ => {}
        }
        if self.guest_to_host(socket) || self.host_to_guest(socket) {
            return true;
        }
        if self.half_closed && self.last_activity.elapsed() >= tuning.half_close_timeout {
            return true;
        }
        self.fin_to_host && self.fin_to_guest && !socket.is_active()
    }

    /// Returns true if the flow must be torn down.
    fn guest_to_host(&mut self, socket: &mut tcp::Socket) -> bool {
        let Ok(permit) = self.to_host.try_reserve() else {
            // Full: leave bytes in the socket so the guest window shrinks.
            // Closed: the host task ended, which `host_to_guest` notices.
            return false;
        };
        if socket.can_recv() {
            if let Ok(chunk) = socket.recv(|data| (data.len(), data.to_vec())) {
                permit.send(ToHost::Data(chunk));
                self.mark_moved();
            }
        } else if !socket.may_recv() && !self.fin_to_host {
            permit.send(ToHost::Fin);
            self.fin_to_host = true;
            self.half_closed = true;
        }
        false
    }

    /// Returns true if the flow must be torn down.
    fn host_to_guest(&mut self, socket: &mut tcp::Socket) -> bool {
        if self.pending.is_none() && !self.host_eof {
            match self.from_host.try_recv() {
                Ok(FromHost::Data(chunk)) => {
                    self.pending = Some(chunk);
                    self.mark_moved();
                }
                Ok(FromHost::Eof) => self.host_eof = true,
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => return true,
            }
        }
        if let Some(chunk) = &mut self.pending
            && socket.can_send()
        {
            let sent = match socket.send_slice(chunk) {
                Ok(sent) => sent,
                Err(_) => return true,
            };
            if sent == chunk.len() {
                self.pending = None;
            } else {
                chunk.drain(..sent);
            }
            if sent > 0 {
                self.mark_moved();
            }
        }
        if self.host_eof && self.pending.is_none() && !self.fin_to_guest && socket.may_send() {
            socket.close();
            self.fin_to_guest = true;
            self.half_closed = true;
        }
        false
    }
}

/// Allocator for the local ports of connections toward the guest.
pub(crate) struct LocalPorts {
    cursor: u16,
    recent: VecDeque<u16>,
}

impl LocalPorts {
    /// Starts at a random port so restarts do not replay the same sequence.
    pub(crate) fn new() -> Self {
        let offset = RandomState::new().hash_one(0u8) % u64::from(EPHEMERAL_PORT_COUNT);
        Self::starting_at(EPHEMERAL_PORT_START + offset as u16)
    }

    fn starting_at(cursor: u16) -> Self {
        LocalPorts {
            cursor,
            recent: VecDeque::with_capacity(RECENT_PORTS),
        }
    }

    /// Picks the next port that is neither `in_use` nor recently retired.
    pub(crate) fn allocate(&mut self, in_use: impl Fn(u16) -> bool) -> u16 {
        loop {
            let port = self.cursor;
            self.cursor = if port == u16::MAX {
                EPHEMERAL_PORT_START
            } else {
                port + 1
            };
            if !in_use(port) && !self.recent.contains(&port) {
                return port;
            }
        }
    }

    /// Holds `port` back from reuse until `RECENT_PORTS` newer ones retire.
    pub(crate) fn retire(&mut self, port: u16) {
        if self.recent.len() == RECENT_PORTS {
            self.recent.pop_front();
        }
        self.recent.push_back(port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_new_allocator_when_allocating_then_port_is_in_dynamic_range() {
        // Arrange
        let mut ports = LocalPorts::new();

        // Act
        let port = ports.allocate(|_| false);

        // Assert
        assert!(port >= EPHEMERAL_PORT_START);
    }

    #[test]
    fn given_retired_port_when_cursor_returns_then_port_is_skipped() {
        // Arrange
        let mut ports = LocalPorts::starting_at(50000);
        let first = ports.allocate(|_| false);
        ports.retire(first);
        ports.cursor = first;

        // Act
        let next = ports.allocate(|_| false);

        // Assert
        assert_eq!(next, first + 1);
    }

    #[test]
    fn given_more_retired_than_memory_when_allocating_then_oldest_is_reusable() {
        // Arrange
        let mut ports = LocalPorts::starting_at(50000);
        for port in 50000..50000 + RECENT_PORTS as u16 + 1 {
            ports.retire(port);
        }
        ports.cursor = 50000;

        // Act
        let next = ports.allocate(|_| false);

        // Assert
        assert_eq!(next, 50000);
    }

    #[test]
    fn given_cursor_at_top_when_allocating_then_it_wraps_to_range_start() {
        // Arrange
        let mut ports = LocalPorts::starting_at(u16::MAX);

        // Act
        let last = ports.allocate(|_| false);
        let wrapped = ports.allocate(|_| false);

        // Assert
        assert_eq!(last, u16::MAX);
        assert_eq!(wrapped, EPHEMERAL_PORT_START);
    }

    #[tokio::test]
    async fn given_flow_dropped_after_fin_when_client_idle_then_host_task_ends() {
        // Arrange
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (to_host, from_host, task) = spawn_host_io(server, Arc::new(Notify::new()));
        to_host.send(ToHost::Fin).await.unwrap();

        // Act
        drop(from_host);

        // Assert: the client never sends, so only cancellation can end the task.
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(ended.is_ok(), "host task should end once the flow is gone");
        drop(client);
    }
}
