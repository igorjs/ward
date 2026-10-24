// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host-to-guest port forwarding tests.
//!
//! The guest is a second smoltcp stack on the far end of the sandbox's
//! socketpair, so every scenario runs real TCP through `spawn_for_sandbox`.

use std::future::Future;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{dhcpv4, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use ward_net::AttachOptions;
use ward_net::inbound::{InboundTuning, MAX_INBOUND_PER_LISTENER, MAX_INBOUND_TOTAL};
use ward_net::smoltcp_backend::{RawFdDevice, SmoltcpHandle, spawn_for_sandbox_with_tuning};

const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GUEST_PORT: u16 = 8080;
const GUEST_TICK: Duration = Duration::from_millis(2);
const GUEST_BUFFER_BYTES: usize = 16384;
const DEADLINE: Duration = Duration::from_secs(20);
const ECHO_BYTES: usize = 1024 * 1024;
const POLL_STEP: Duration = Duration::from_millis(10);
const SHORT_TIMEOUT: Duration = Duration::from_millis(400);
const FAIL_FAST: Duration = Duration::from_secs(3);

/// What the fake guest does with its one connection, called every tick.
type Behavior = Box<dyn FnMut(&mut tcp::Socket) + Send>;

/// Tuning whose connect timeout outlives any pending-connection test.
fn long_connect_tuning() -> InboundTuning {
    InboundTuning {
        connect_timeout: DEADLINE,
        ..InboundTuning::default()
    }
}

fn always_allow(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { true })
}

async fn spawn_net() -> SmoltcpHandle {
    spawn_net_with(InboundTuning::default()).await
}

async fn spawn_net_with(tuning: InboundTuning) -> SmoltcpHandle {
    spawn_for_sandbox_with_tuning(
        "inbound-test",
        &AttachOptions::default(),
        Box::new(always_allow),
        tuning,
    )
    .await
    .expect("spawn_for_sandbox")
}

/// Polls `condition` until it holds or `DEADLINE` passes.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(POLL_STEP).await;
    }
}

async fn wait_for_lease(handle: &SmoltcpHandle) {
    eventually("guest DHCP lease", || handle.guest_addr().is_some()).await;
}

/// How the fake guest treats TCP connections.
#[derive(Clone, Copy, PartialEq)]
enum GuestMode {
    /// One socket listens on `GUEST_PORT`.
    Listen,
    /// Nothing listens, so the stack answers SYNs with RST.
    Refuse,
}

struct Guest {
    task: JoinHandle<()>,
    mute: Arc<AtomicBool>,
}

impl Guest {
    /// Stops the guest answering anything while keeping its fd open, so
    /// the host's SYNs go unanswered.
    fn go_silent(&self) {
        self.mute.store(true, Ordering::Relaxed);
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Runs a guest stack on the handle's guest fd. It asks for a DHCP lease
/// first, since the host only forwards to a guest that holds one.
fn spawn_guest(fd: i32, mode: GuestMode, mut behavior: Behavior) -> Guest {
    // SAFETY: the fd comes from the handle's socketpair and is owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mute = Arc::new(AtomicBool::new(false));
    let muted = Arc::clone(&mute);
    let task = tokio::spawn(async move {
        let mut device = RawFdDevice::new(fd);
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(GUEST_MAC)),
            &mut device,
            Instant::now(),
        );
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GUEST_ADDR), 24))
                .unwrap();
        });
        let mut sockets = SocketSet::new(Vec::new());
        sockets.add(dhcpv4::Socket::new());
        let handle = sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; GUEST_BUFFER_BYTES]),
            tcp::SocketBuffer::new(vec![0u8; GUEST_BUFFER_BYTES]),
        ));
        loop {
            if !muted.load(Ordering::Relaxed) {
                iface.poll(Instant::now(), &mut device, &mut sockets);
                let socket = sockets.get_mut::<tcp::Socket>(handle);
                if mode == GuestMode::Listen {
                    if !socket.is_open() {
                        socket.listen(GUEST_PORT).unwrap();
                    }
                    behavior(socket);
                }
            }
            tokio::time::sleep(GUEST_TICK).await;
        }
    });
    Guest { task, mute }
}

/// Guest that reads a request and answers once it has `want` bytes.
fn reply_after(want: usize, reply: &'static [u8], seen: Arc<Mutex<Vec<u8>>>) -> Behavior {
    let mut replied = false;
    Box::new(move |socket| {
        if socket.can_recv() {
            let _ = socket.recv(|data| {
                seen.lock().unwrap().extend_from_slice(data);
                (data.len(), ())
            });
        }
        if !replied && seen.lock().unwrap().len() >= want && socket.can_send() {
            socket.send_slice(reply).unwrap();
            replied = true;
        }
    })
}

/// Guest that echoes everything and closes once the host closes.
fn echo() -> Behavior {
    let mut backlog: Vec<u8> = Vec::new();
    Box::new(move |socket| {
        if socket.can_recv() && backlog.len() < GUEST_BUFFER_BYTES {
            let _ = socket.recv(|data| {
                backlog.extend_from_slice(data);
                (data.len(), ())
            });
        }
        if !backlog.is_empty() && socket.can_send() {
            let sent = socket.send_slice(&backlog).unwrap();
            backlog.drain(..sent);
        }
        if backlog.is_empty() && !socket.may_recv() && socket.may_send() {
            socket.close();
        }
    })
}

/// Publishes `GUEST_PORT` once the guest holds a lease.
async fn publish(handle: &SmoltcpHandle) -> ward_net::inbound::InboundHandle {
    wait_for_lease(handle).await;
    handle.add_inbound_loopback(0, GUEST_PORT).await.unwrap()
}

async fn dial(addr: SocketAddr) -> TcpStream {
    tokio::time::timeout(DEADLINE, TcpStream::connect(addr))
        .await
        .expect("connect timed out")
        .expect("connect")
}

async fn read_to_end(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(DEADLINE, stream.read_to_end(&mut out))
        .await
        .expect("read timed out")
        .expect("read");
    out
}

#[tokio::test]
async fn given_guest_server_when_host_client_sends_then_guest_receives_and_replies() {
    // Arrange
    let handle = spawn_net().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        reply_after(4, b"pong", seen.clone()),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    client.write_all(b"ping").await.unwrap();
    let mut reply = [0u8; 4];
    tokio::time::timeout(DEADLINE, client.read_exact(&mut reply))
        .await
        .expect("reply timed out")
        .unwrap();

    // Assert
    assert_eq!(&reply, b"pong");
    assert_eq!(seen.lock().unwrap().as_slice(), b"ping");
    assert!(forward.local_addr().ip().is_loopback());
    assert_ne!(forward.local_addr().port(), 0);
}

#[tokio::test]
async fn given_dropped_handle_when_connecting_then_refused() {
    // Arrange
    let handle = spawn_net().await;
    let forward = handle.add_inbound_loopback(0, GUEST_PORT).await.unwrap();
    let addr = forward.local_addr();

    // Act
    drop(forward);

    // Assert: the abort lands on the next scheduler turn, so poll.
    let refused = tokio::time::timeout(DEADLINE, async {
        loop {
            match TcpStream::connect(addr).await {
                Err(err) if err.kind() == ErrorKind::ConnectionRefused => return,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await;
    assert!(refused.is_ok(), "listener should be closed after drop");
}

#[tokio::test]
async fn given_open_connections_when_handle_dropped_then_clients_are_closed() {
    // Arrange
    let handle = spawn_net().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        reply_after(1, b"x", seen),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;
    client.write_all(b"a").await.unwrap();
    let mut byte = [0u8; 1];
    client.read_exact(&mut byte).await.unwrap();

    // Act
    drop(forward);

    // Assert
    let mut rest = Vec::new();
    let end = tokio::time::timeout(DEADLINE, client.read_to_end(&mut rest)).await;
    assert!(end.is_ok(), "open connection should end after handle drop");
}

#[tokio::test]
async fn given_listener_at_cap_when_one_more_connects_then_it_is_closed() {
    // Arrange: a silent guest leaves accepted connections pending.
    let handle = spawn_net_with(long_connect_tuning()).await;
    let guest = spawn_guest(handle.guest_fd, GuestMode::Refuse, Box::new(|_| {}));
    let forward = publish(&handle).await;
    guest.go_silent();
    let mut held = Vec::new();
    for _ in 0..MAX_INBOUND_PER_LISTENER {
        held.push(dial(forward.local_addr()).await);
    }

    // Act: accepts are FIFO, so the extra one is taken after every held one.
    let mut extra = dial(forward.local_addr()).await;

    // Assert
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(DEADLINE, extra.read(&mut buf))
        .await
        .expect("over-cap connection should be closed promptly");
    assert!(matches!(outcome, Ok(0) | Err(_)));
    let mut first = held.remove(0);
    let still_open = tokio::time::timeout(Duration::from_millis(200), first.read(&mut buf)).await;
    assert!(still_open.is_err(), "connections under the cap stay open");
}

#[tokio::test]
async fn given_sandbox_at_total_cap_when_one_more_connects_then_it_is_closed() {
    // Arrange: enough listeners to fill the sandbox-wide cap.
    let handle = spawn_net_with(long_connect_tuning()).await;
    let guest = spawn_guest(handle.guest_fd, GuestMode::Refuse, Box::new(|_| {}));
    let mut forwards = Vec::new();
    for _ in 0..=MAX_INBOUND_TOTAL / MAX_INBOUND_PER_LISTENER {
        forwards.push(publish(&handle).await);
    }
    guest.go_silent();
    let mut held = Vec::new();
    for forward in &forwards[..MAX_INBOUND_TOTAL / MAX_INBOUND_PER_LISTENER] {
        for _ in 0..MAX_INBOUND_PER_LISTENER {
            held.push(dial(forward.local_addr()).await);
        }
    }
    eventually("every held connection accepted", || {
        handle.inbound_in_use() == MAX_INBOUND_TOTAL
    })
    .await;

    // Act
    let spare = forwards.last().unwrap();
    let mut extra = dial(spare.local_addr()).await;

    // Assert
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(DEADLINE, extra.read(&mut buf))
        .await
        .expect("over-cap connection should be closed promptly");
    assert!(matches!(outcome, Ok(0) | Err(_)));
    assert_eq!(handle.inbound_in_use(), MAX_INBOUND_TOTAL);
}

#[tokio::test]
async fn given_silent_guest_when_connect_timeout_passes_then_client_is_reset() {
    // Arrange
    let handle = spawn_net_with(InboundTuning {
        connect_timeout: SHORT_TIMEOUT,
        ..InboundTuning::default()
    })
    .await;
    let guest = spawn_guest(handle.guest_fd, GuestMode::Refuse, Box::new(|_| {}));
    let forward = publish(&handle).await;
    guest.go_silent();
    let mut client = dial(forward.local_addr()).await;
    let started = StdInstant::now();

    // Act
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(FAIL_FAST, client.read(&mut buf))
        .await
        .expect("connect timeout should reset the client");

    // Assert
    assert_eq!(outcome.unwrap_err().kind(), ErrorKind::ConnectionReset);
    assert!(started.elapsed() >= SHORT_TIMEOUT / 2);
}

#[tokio::test]
async fn given_no_dhcp_lease_when_host_connects_then_client_is_reset_quickly() {
    // Arrange: no guest runs, so no lease is ever granted.
    let handle = spawn_net().await;
    let forward = handle.add_inbound_loopback(0, GUEST_PORT).await.unwrap();
    let mut client = dial(forward.local_addr()).await;

    // Act
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(FAIL_FAST, client.read(&mut buf))
        .await
        .expect("a guest without a lease should fail fast");

    // Assert
    assert_eq!(outcome.unwrap_err().kind(), ErrorKind::ConnectionReset);
    assert_eq!(handle.guest_addr(), None);
}

#[tokio::test]
async fn given_half_closed_client_when_guest_keeps_streaming_then_flow_outlives_timeout() {
    // Arrange: the guest sends a byte every 50 ms for several timeouts.
    const CHUNKS: usize = 24;
    const CHUNK_GAP: Duration = Duration::from_millis(50);
    let handle = spawn_net_with(InboundTuning {
        half_close_timeout: SHORT_TIMEOUT,
        ..InboundTuning::default()
    })
    .await;
    let mut sent = 0;
    let mut last = StdInstant::now();
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        Box::new(move |socket| {
            if socket.can_recv() {
                let _ = socket.recv(|data| (data.len(), ()));
            }
            if sent < CHUNKS && last.elapsed() >= CHUNK_GAP && socket.can_send() {
                socket.send_slice(b"x").unwrap();
                sent += 1;
                last = StdInstant::now();
            } else if sent == CHUNKS && socket.may_send() {
                socket.close();
            }
        }),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    client.shutdown().await.unwrap();
    let streamed = read_to_end(&mut client).await;

    // Assert
    assert_eq!(streamed.len(), CHUNKS);
}

#[tokio::test]
async fn given_half_closed_client_when_guest_stays_idle_then_flow_is_reset_after_timeout() {
    // Arrange: the guest never answers or closes.
    let handle = spawn_net_with(InboundTuning {
        half_close_timeout: SHORT_TIMEOUT,
        ..InboundTuning::default()
    })
    .await;
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        Box::new(|socket| {
            if socket.can_recv() {
                let _ = socket.recv(|data| (data.len(), ()));
            }
        }),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    client.shutdown().await.unwrap();
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(FAIL_FAST, client.read(&mut buf))
        .await
        .expect("an idle half-closed flow should be reset");

    // Assert
    assert_eq!(outcome.unwrap_err().kind(), ErrorKind::ConnectionReset);
}

#[tokio::test]
async fn given_host_shutdown_write_when_guest_sees_eof_then_guest_can_still_reply() {
    // Arrange
    let handle = spawn_net().await;
    let mut got_eof = false;
    let mut sent = false;
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        Box::new(move |socket| {
            if socket.can_recv() {
                let _ = socket.recv(|data| (data.len(), ()));
            }
            got_eof |= !socket.may_recv() && socket.state() == tcp::State::CloseWait;
            if got_eof && !sent && socket.can_send() {
                socket.send_slice(b"done").unwrap();
                socket.close();
                sent = true;
            }
        }),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    client.write_all(b"req").await.unwrap();
    client.shutdown().await.unwrap();

    // Assert
    assert_eq!(read_to_end(&mut client).await, b"done");
}

#[tokio::test]
async fn given_guest_closes_its_side_when_host_keeps_sending_then_guest_still_receives() {
    // Arrange
    let handle = spawn_net().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let mut sent = false;
    let _guest = spawn_guest(
        handle.guest_fd,
        GuestMode::Listen,
        Box::new(move |socket| {
            if socket.can_recv() {
                let _ = socket.recv(|data| {
                    log.lock().unwrap().extend_from_slice(data);
                    (data.len(), ())
                });
            }
            if !sent && socket.can_send() {
                socket.send_slice(b"bye").unwrap();
                socket.close();
                sent = true;
            }
        }),
    );
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    let mut greeting = [0u8; 3];
    tokio::time::timeout(DEADLINE, client.read_exact(&mut greeting))
        .await
        .expect("greeting timed out")
        .unwrap();
    client.write_all(b"late").await.unwrap();

    // Assert
    assert_eq!(&greeting, b"bye");
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while seen.lock().unwrap().len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "guest never got bytes"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen.lock().unwrap().as_slice(), b"late");
}

#[tokio::test]
async fn given_one_mebibyte_when_echoed_through_guest_then_bytes_match() {
    // Arrange
    let handle = spawn_net().await;
    let _guest = spawn_guest(handle.guest_fd, GuestMode::Listen, echo());
    let forward = publish(&handle).await;
    let payload: Vec<u8> = (0..ECHO_BYTES).map(|i| (i % 251) as u8).collect();
    let client = dial(forward.local_addr()).await;
    let (mut read_half, mut write_half) = client.into_split();
    let to_send = payload.clone();

    // Act
    let writer = tokio::spawn(async move {
        write_half.write_all(&to_send).await.unwrap();
        write_half.shutdown().await.unwrap();
    });
    let mut echoed = Vec::new();
    tokio::time::timeout(DEADLINE, read_half.read_to_end(&mut echoed))
        .await
        .expect("echo timed out")
        .unwrap();
    writer.await.unwrap();

    // Assert
    assert_eq!(echoed.len(), payload.len());
    assert!(echoed == payload, "echoed bytes differ from sent bytes");
}

#[tokio::test]
async fn given_guest_not_listening_when_host_connects_then_reset_reaches_client() {
    // Arrange
    let handle = spawn_net().await;
    let _guest = spawn_guest(handle.guest_fd, GuestMode::Refuse, Box::new(|_| {}));
    let forward = publish(&handle).await;
    let mut client = dial(forward.local_addr()).await;

    // Act
    let mut buf = [0u8; 1];
    let outcome = tokio::time::timeout(DEADLINE, client.read(&mut buf))
        .await
        .expect("reset should arrive before the deadline");

    // Assert
    let err = outcome.expect_err("client should see a reset, not a clean close");
    assert_eq!(err.kind(), ErrorKind::ConnectionReset);
}

#[tokio::test]
async fn given_a_busy_host_port_when_publishing_then_the_error_says_address_in_use() {
    // Arrange
    let handle = spawn_net().await;
    let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let busy = occupied.local_addr().unwrap().port();

    // Act
    let err = handle
        .add_inbound_loopback(busy, GUEST_PORT)
        .await
        .err()
        .expect("bind should fail");

    // Assert
    assert!(
        matches!(&err, ward_net::Error::Listen { source, .. } if source.kind() == ErrorKind::AddrInUse),
        "{err:?}"
    );
    assert!(err.to_string().contains(&busy.to_string()), "{err}");
}

#[tokio::test]
async fn given_a_closed_forward_when_the_port_is_bound_again_then_it_is_free_at_once() {
    // Arrange
    let handle = spawn_net().await;
    let forward = handle.add_inbound_loopback(0, GUEST_PORT).await.unwrap();
    let port = forward.local_addr().port();

    // Act
    forward.close().await;

    // Assert
    assert!(handle.add_inbound_loopback(port, GUEST_PORT).await.is_ok());
}

#[tokio::test]
async fn given_production_spawn_when_no_dhcp_lease_then_guest_addr_is_the_static_guest_address() {
    // Arrange
    let mut handle = ward_net::smoltcp_backend::spawn_for_sandbox(
        "inbound-test",
        &AttachOptions::default(),
        Box::new(always_allow),
    )
    .await
    .unwrap();

    // Act
    let addr = handle.guest_addr();

    // Assert
    assert_eq!(addr, Some(GUEST_ADDR));
    handle.detach().await.unwrap();
}
