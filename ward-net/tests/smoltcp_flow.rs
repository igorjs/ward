// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! TCP flow table tests for `Stack`.
//!
//! Drives a guest-side TCP SYN through the raw socketpair harness and
//! asserts `Stack` opens the outbound connection via an injected
//! `Connector` rather than dialing the network itself, so a test never
//! needs live network access to prove the flow table's addressing logic.

#![cfg(feature = "smoltcp")]

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    TcpSeqNumber,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use ward_net::smoltcp_backend::{Connector, Resolver, Stack};

/// MAC `Stack`'s interface already answers on (mirrors the private
/// `INTERFACE_HARDWARE_ADDR` constant in `smoltcp_backend`), so a guest
/// frame addressed here is accepted instead of dropped as a MAC mismatch.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface must own for a guest frame addressed
/// here (only used for the ARP handshake below) to be accepted rather
/// than dropped.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Pre-DHCP guest address for this test; DHCP itself is a separate
/// scenario, so this is only ever used as the SYN's source address.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// Source port the guest's SYN carries.
const GUEST_SRC_PORT: u16 = 55555;
/// Destination this scenario's SYN targets: a real but unrelated public IP
/// used only as an address label, never actually dialed since the fake
/// `Connector` intercepts every connect attempt.
const DEST_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
const DEST_PORT: u16 = 80;
/// Expected cap on `Stack::flows`, same order of magnitude as
/// `smoltcp_dns.rs`'s `MAX_RESOLVED_ENTRIES`, so the capacity test below can
/// assert against the real limit once it exists, without the production
/// constant needing to be exported.
const MAX_FLOW_ENTRIES: usize = 1024;

/// `Stack::new` requires a `Resolver`, but this scenario never sends a DNS
/// query, so this always answers empty rather than standing in for a real
/// lookup.
struct NullResolver;

#[async_trait::async_trait]
impl Resolver for NullResolver {
    async fn resolve(&self, _name: &str) -> Vec<IpAddr> {
        Vec::new()
    }
}

/// Fake `Connector` that records the single address it was called with, so
/// a test can assert the flow table dispatched the guest's own SYN
/// destination rather than some other address.
struct RecordingConnector {
    called_with: Arc<Mutex<Option<SocketAddr>>>,
}

#[async_trait::async_trait]
impl Connector for RecordingConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        *self.called_with.lock().unwrap() = Some(addr);
        Err(io::Error::other(
            "RecordingConnector never dials out; this scenario only proves it was called",
        ))
    }
}

/// Create an `AF_UNIX SOCK_DGRAM` pair and return both ends as owned fds.
///
/// Mirrors `smoltcp_dns.rs`'s helper of the same shape.
fn socketpair_dgram() -> (OwnedFd, OwnedFd) {
    let mut sv: [std::ffi::c_int; 2] = [-1, -1];
    // SAFETY: socketpair is a pure syscall with no preconditions beyond a
    // valid `sv` pointer; both fds are closed on drop via OwnedFd.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) };
    assert_eq!(
        ret,
        0,
        "socketpair(AF_UNIX, SOCK_DGRAM) failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: socketpair succeeded; sv[0] and sv[1] are valid open fds.
    let a = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let b = unsafe { OwnedFd::from_raw_fd(sv[1]) };
    (a, b)
}

/// Write `frame` as a single datagram on `fd`.
fn write_frame(fd: &OwnedFd, frame: &[u8]) {
    // SAFETY: fd is a valid open socket for the duration of this call;
    // frame's pointer and length describe a valid, initialized slice.
    let ret = unsafe { libc::write(fd.as_raw_fd(), frame.as_ptr().cast(), frame.len()) };
    assert_eq!(
        ret,
        frame.len() as isize,
        "write to socketpair should write the whole frame in one datagram: {}",
        std::io::Error::last_os_error()
    );
}

/// Non-blocking single-datagram read, `None` if nothing is queued yet.
fn try_read_frame(fd: &OwnedFd) -> Option<Vec<u8>> {
    let mut buf = [0u8; 1514];
    // SAFETY: fd is a valid open socket for the duration of this call; buf
    // is a valid, initialized buffer of the given length. MSG_DONTWAIT
    // makes this non-blocking so an empty socket returns immediately.
    let n = unsafe {
        libc::recv(
            fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n > 0 {
        Some(buf[..n as usize].to_vec())
    } else {
        None
    }
}

/// Sends an ARP request for `GATEWAY_ADDR` from the guest and drains the
/// reply, so `stack`'s neighbor cache learns the guest's MAC before a
/// scenario needs a unicast reply routed back to it. Mirrors
/// `smoltcp_dns.rs`'s helper of the same shape.
async fn perform_arp_handshake(stack: &mut Stack, guest_fd: &OwnedFd) {
    let arp_repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: GUEST_MAC,
        source_protocol_addr: GUEST_ADDR,
        target_hardware_addr: EthernetAddress([0, 0, 0, 0, 0, 0]),
        target_protocol_addr: GATEWAY_ADDR,
    };
    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    };

    let total_len = eth_repr.buffer_len() + arp_repr.buffer_len();
    let mut buf = vec![0u8; total_len];
    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth_frame);
    let mut arp_packet = ArpPacket::new_unchecked(eth_frame.payload_mut());
    arp_repr.emit(&mut arp_packet);

    write_frame(guest_fd, &buf);

    // The reply may only flush on a later tick, same as any other response.
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();

    try_read_frame(guest_fd).expect("stack should reply to the guest's ARP request");
}

/// Builds a complete Ethernet+IPv4+TCP SYN frame a guest would send to open
/// a connection to `DEST_ADDR:DEST_PORT`, with a correctly computed
/// IPv4/TCP checksum so the checksum validation smoltcp runs by default on
/// ingress accepts it.
fn build_tcp_syn_frame() -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port: GUEST_SRC_PORT,
        dst_port: DEST_PORT,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(0),
        ack_number: None,
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    let tcp_len = tcp_repr.buffer_len();

    let ip_repr = Ipv4Repr {
        src_addr: GUEST_ADDR,
        dst_addr: DEST_ADDR,
        next_header: IpProtocol::Tcp,
        payload_len: tcp_len,
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: GATEWAY_MAC,
        ethertype: EthernetProtocol::Ipv4,
    };

    let total_len = eth_repr.buffer_len() + ip_repr.buffer_len() + tcp_len;
    let mut buf = vec![0u8; total_len];

    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth_frame);

    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());

    let mut tcp_packet = TcpPacket::new_unchecked(ip_packet.payload_mut());
    tcp_repr.emit(
        &mut tcp_packet,
        &IpAddress::Ipv4(GUEST_ADDR),
        &IpAddress::Ipv4(DEST_ADDR),
        &ChecksumCapabilities::default(),
    );

    buf
}

/// Polls `stack` on a short cadence until `called_with` records an
/// address or `budget` elapses, to tolerate the connector being invoked
/// from a task spawned on a later poll tick rather than synchronously.
async fn poll_until_connector_called(
    stack: &mut Stack,
    called_with: &Arc<Mutex<Option<SocketAddr>>>,
    budget: Duration,
) -> Option<SocketAddr> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        if let Some(addr) = *called_with.lock().unwrap() {
            return Some(addr);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn given_guest_syn_to_open_dest_when_polled_then_fake_connector_called_with_correct_addr() {
    // Arrange: a Stack backed by the socketpair harness and a fake
    // Connector that records the address it is asked to dial instead of
    // opening a real connection.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let called_with = Arc::new(Mutex::new(None));
    let connector: Box<dyn Connector> = Box::new(RecordingConnector {
        called_with: Arc::clone(&called_with),
    });
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let syn_frame = build_tcp_syn_frame();
    write_frame(&guest_fd, &syn_frame);

    // Act
    let addr = poll_until_connector_called(&mut stack, &called_with, Duration::from_secs(2))
        .await
        .expect("stack should call the connector for the guest's SYN within the poll budget");

    // Assert
    assert_eq!(
        addr,
        SocketAddr::new(IpAddr::V4(DEST_ADDR), DEST_PORT),
        "connector should be called with the exact destination address the guest's SYN carried"
    );
}

/// Fake `Connector` that ignores the guest's requested destination and
/// always dials the fixed loopback listener this test controls, standing in
/// for a real outbound connection without needing live network access.
struct LoopbackConnector {
    listener_addr: SocketAddr,
}

#[async_trait::async_trait]
impl Connector for LoopbackConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        TcpStream::connect(self.listener_addr).await
    }
}

/// TCP header fields parsed out of a reply frame the stack sent back to the
/// guest, the minimum this scenario needs to react to the handshake
/// smoltcp's own `tcp::Socket` drives once `listen()`'d.
struct ParsedTcpSegment {
    control: TcpControl,
    seq_number: TcpSeqNumber,
    payload: Vec<u8>,
}

/// Parses `frame` as an Ethernet+IPv4+TCP frame, panicking if it isn't one:
/// a malformed frame here is a test bug, not something worth handling
/// gracefully.
fn parse_tcp_segment(frame: &[u8]) -> ParsedTcpSegment {
    let eth_frame = EthernetFrame::new_checked(frame).expect("valid ethernet frame");
    let ip_packet = Ipv4Packet::new_checked(eth_frame.payload()).expect("valid ipv4 packet");
    let ip_repr =
        Ipv4Repr::parse(&ip_packet, &ChecksumCapabilities::default()).expect("valid ipv4 repr");
    let tcp_packet = TcpPacket::new_checked(ip_packet.payload()).expect("valid tcp packet");
    let tcp_repr = TcpRepr::parse(
        &tcp_packet,
        &IpAddress::Ipv4(ip_repr.src_addr),
        &IpAddress::Ipv4(ip_repr.dst_addr),
        &ChecksumCapabilities::default(),
    )
    .expect("valid tcp repr");
    ParsedTcpSegment {
        control: tcp_repr.control,
        seq_number: tcp_repr.seq_number,
        payload: tcp_repr.payload.to_vec(),
    }
}

/// Builds a plain (non-SYN) Ethernet+IPv4+TCP frame from the guest,
/// acknowledging `ack` and carrying `payload`. Used both for the ACK that
/// completes the handshake (empty payload) and for the data segment sent
/// afterwards.
fn build_tcp_ack_frame(seq: TcpSeqNumber, ack: TcpSeqNumber, payload: &[u8]) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port: GUEST_SRC_PORT,
        dst_port: DEST_PORT,
        control: TcpControl::None,
        seq_number: seq,
        ack_number: Some(ack),
        window_len: 65535,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload,
    };
    let tcp_len = tcp_repr.buffer_len();

    let ip_repr = Ipv4Repr {
        src_addr: GUEST_ADDR,
        dst_addr: DEST_ADDR,
        next_header: IpProtocol::Tcp,
        payload_len: tcp_len,
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: GATEWAY_MAC,
        ethertype: EthernetProtocol::Ipv4,
    };

    let total_len = eth_repr.buffer_len() + ip_repr.buffer_len() + tcp_len;
    let mut buf = vec![0u8; total_len];

    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth_frame);

    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());

    let mut tcp_packet = TcpPacket::new_unchecked(ip_packet.payload_mut());
    tcp_repr.emit(
        &mut tcp_packet,
        &IpAddress::Ipv4(GUEST_ADDR),
        &IpAddress::Ipv4(DEST_ADDR),
        &ChecksumCapabilities::default(),
    );

    buf
}

/// Polls `stack` until a reply frame carrying the SYN control flag appears
/// on `guest_fd`, or `budget` elapses. Mirrors
/// `poll_until_connector_called`'s polling shape: the reply may only flush
/// on a later tick.
async fn poll_until_syn_ack(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Option<ParsedTcpSegment> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        while let Some(frame) = try_read_frame(guest_fd) {
            let segment = parse_tcp_segment(&frame);
            if segment.control == TcpControl::Syn {
                return Some(segment);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Non-blocking read of whatever bytes are currently available on `stream`,
/// `None` if nothing has arrived yet. Uses a raw `recv(2)` rather than
/// tokio's `AsyncRead`, mirroring this file's other socket helpers instead
/// of pulling in another tokio IO feature just for this one check.
fn try_read_stream_bytes(stream: &TcpStream) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    // SAFETY: stream's fd is a valid open socket for the duration of this
    // call; buf is a valid, initialized buffer of the given length.
    // MSG_DONTWAIT makes this non-blocking so an empty socket returns
    // immediately instead of parking the test.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n > 0 {
        Some(buf[..n as usize].to_vec())
    } else {
        None
    }
}

/// Polls `stack` until bytes are readable on `stream`, or `budget` elapses.
async fn poll_until_stream_bytes(
    stack: &mut Stack,
    stream: &TcpStream,
    budget: Duration,
) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        if let Some(bytes) = try_read_stream_bytes(stream) {
            return Some(bytes);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Polls `stack` until a TCP segment carrying a non-empty payload appears
/// on `guest_fd`, or `budget` elapses. Any earlier control-only segment
/// (e.g. the SYN-ACK or a bare ACK) is skipped rather than returned.
async fn poll_until_guest_payload(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        while let Some(frame) = try_read_frame(guest_fd) {
            let segment = parse_tcp_segment(&frame);
            if !segment.payload.is_empty() {
                return Some(segment.payload);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn given_established_flow_when_guest_sends_bytes_then_host_receives_them() {
    // Arrange: a Stack backed by the socketpair harness and a fake
    // Connector that dials a real TcpListener this test controls, so the
    // host side of the connection is a real, readable socket.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("binding a loopback listener on an ephemeral port should not fail");
    let listener_addr = listener
        .local_addr()
        .expect("a bound listener should report its own local address");
    let (host_stream_tx, host_stream_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (host_stream, _) = listener
            .accept()
            .await
            .expect("listener should accept the fake connector's dial");
        let _ = host_stream_tx.send(host_stream);
    });

    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: drive a full handshake from the guest (SYN, then the ACK that
    // completes it once the stack's own tcp::Socket answers with a
    // SYN-ACK), then send bytes over the resulting established connection.
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack's own tcp::Socket should reply with a SYN-ACK once listen()'d");

    let client_next_seq = TcpSeqNumber(1); // the guest's SYN consumed one sequence number
    let server_next_seq = syn_ack.seq_number + 1;
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(client_next_seq, server_next_seq, &[]),
    );
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();

    let payload = b"hello from guest";
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(client_next_seq, server_next_seq, payload),
    );

    let host_stream = tokio::time::timeout(Duration::from_secs(2), host_stream_rx)
        .await
        .expect("connector should have connected to the listener within the timeout")
        .expect("host stream sender should not be dropped before sending");

    // Assert: the connector's real TcpStream on the host side should
    // receive the bytes the guest just sent over the now-established flow.
    let received = poll_until_stream_bytes(&mut stack, &host_stream, Duration::from_secs(2))
        .await
        .expect("host should receive the guest's bytes once the flow is established");
    assert_eq!(
        received, payload,
        "host side of the connection should receive exactly the bytes the guest sent"
    );
}

#[tokio::test]
async fn given_established_flow_when_host_sends_bytes_then_guest_receives_them() {
    // Arrange: same handshake setup as the guest->host scenario, but this
    // time the test writes to the host side of the accepted connection.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("binding a loopback listener on an ephemeral port should not fail");
    let listener_addr = listener
        .local_addr()
        .expect("a bound listener should report its own local address");
    let (host_stream_tx, host_stream_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (host_stream, _) = listener
            .accept()
            .await
            .expect("listener should accept the fake connector's dial");
        let _ = host_stream_tx.send(host_stream);
    });

    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: drive a full handshake from the guest (SYN, then the ACK that
    // completes it), then have the host side of the connection write bytes.
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack's own tcp::Socket should reply with a SYN-ACK once listen()'d");

    let client_next_seq = TcpSeqNumber(1); // the guest's SYN consumed one sequence number
    let server_next_seq = syn_ack.seq_number + 1;
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(client_next_seq, server_next_seq, &[]),
    );
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();

    let host_stream = tokio::time::timeout(Duration::from_secs(2), host_stream_rx)
        .await
        .expect("connector should have connected to the listener within the timeout")
        .expect("host stream sender should not be dropped before sending");

    let payload = b"hello from host";
    host_stream
        .try_write(payload)
        .expect("writing to a freshly accepted loopback stream should not block");

    // Assert: the guest side of the raw device should receive a TCP
    // segment carrying exactly the bytes the host just wrote.
    let received = poll_until_guest_payload(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("guest should receive the host's bytes once the flow is established");
    assert_eq!(
        received, payload,
        "guest side of the connection should receive exactly the bytes the host sent"
    );
}

/// Fake `Connector` whose `connect` future never resolves, standing in for
/// an unresponsive remote host without ever blocking on real network I/O:
/// the flow should still time out and reset rather than stay `Connecting`
/// forever.
struct HangingConnector;

#[async_trait::async_trait]
impl Connector for HangingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        std::future::pending().await
    }
}

/// Polls `stack` until a reply frame carrying the RST control flag appears
/// on `guest_fd`, or `budget` (real wall-clock time) elapses. Observing an
/// RST from the guest's side is how this scenario checks the flow was torn
/// down, since `Stack` exposes no other way to inspect the flow table's
/// contents from outside `smoltcp_backend`.
async fn poll_until_rst(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Option<ParsedTcpSegment> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        while let Some(frame) = try_read_frame(guest_fd) {
            let segment = parse_tcp_segment(&frame);
            if segment.control == TcpControl::Rst {
                return Some(segment);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test(start_paused = true)]
async fn given_fake_connector_future_never_resolves_when_polled_then_flow_times_out_and_resets_within_timeout_window()
 {
    // Arrange: a Stack backed by a connector whose connect future never
    // resolves, standing in for an unresponsive remote host.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(HangingConnector);
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: the guest's SYN starts a connect attempt that will never
    // resolve, then fast-forward the mocked clock well past any reasonable
    // connect-attempt timeout, using the paused clock rather than a real
    // sleep so the test stays fast regardless of the timeout's length.
    write_frame(&guest_fd, &build_tcp_syn_frame());
    stack.poll();
    tokio::time::advance(Duration::from_secs(60)).await;

    // Assert: the flow should be reset rather than left stuck `Connecting`
    // forever. `poll_until_rst`'s budget is real wall-clock time, so a
    // still-missing timeout fails this test quickly instead of hanging the
    // suite.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_millis(200)).await;
    assert!(
        rst.is_some(),
        "guest should receive an RST once the connect attempt times out, but the flow was never reset"
    );
}

#[tokio::test]
async fn given_flow_table_at_capacity_when_new_syn_then_rejected_with_rst() {
    // Arrange: fill Stack's flow table to its cap with synthetic entries
    // inserted directly, mirroring the DNS relay's resolved-map capacity
    // test, so reaching the cap doesn't require driving thousands of real
    // SYNs through the harness. Each synthetic entry targets a distinct
    // destination port on an address the real SYN below never uses, so it
    // can never collide with the flow the test actually cares about.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let called_with = Arc::new(Mutex::new(None));
    let connector: Box<dyn Connector> = Box::new(RecordingConnector {
        called_with: Arc::clone(&called_with),
    });
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let synthetic_dest = Ipv4Addr::new(198, 51, 100, 1);
    for i in 0..MAX_FLOW_ENTRIES {
        let src = SocketAddr::new(IpAddr::V4(GUEST_ADDR), 20_000 + i as u16);
        let dst = SocketAddr::new(IpAddr::V4(synthetic_dest), (i + 1) as u16);
        stack.insert_synthetic_flow_for_test(src, dst);
    }

    // Act: the guest sends a SYN for a destination never seen before, with
    // the flow table already holding exactly its cap's worth of entries.
    write_frame(&guest_fd, &build_tcp_syn_frame());

    // Assert: the new SYN is rejected with an RST rather than silently
    // dropped or accepted past the cap, and the connector is never
    // dispatched for a flow the table had no room to admit.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "a new guest SYN arriving once the flow table is at capacity should be \
         rejected with an RST"
    );
    assert!(
        called_with.lock().unwrap().is_none(),
        "the connector should never be dispatched for a SYN the flow table had \
         no room to admit"
    );
}

#[tokio::test]
async fn given_host_closes_connection_when_flow_torn_down_then_second_poll_does_not_panic() {
    // Arrange: a Stack backed by a real TcpListener standing in for the
    // host peer. spawn_host_io's write and read loops race in one
    // tokio::select!, so whichever ends first drops both channel halves
    // (to_host_rx and from_host_tx) together; closing the host's end of
    // the connection makes the read loop see EOF and end the task.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let listener_addr = listener.local_addr().expect("local_addr");
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let mut stack = Stack::new(host_fd, resolver, connector);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: establish the flow, then close the host side immediately so
    // its io task ends and both channel halves drop in the same tick.
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        syn_ack.is_some(),
        "flow must reach Established before this scenario can exercise teardown"
    );
    let (host_stream, _) = listener.accept().await.expect("accept");
    drop(host_stream);

    // poll_until_rst polls in a loop until the flow is actually torn
    // down (previously queuing the flow for removal twice in that same
    // pass: the write channel's Closed arm and the read channel's
    // Disconnected arm both fire once the io task ends). This confirms
    // teardown happened before the assertion below exercises the panic.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "flow should be torn down (RST to the guest) once the host closes its side"
    );

    // Assert: one more poll must not panic. This is where
    // `sockets_pending_removal` gets drained; a duplicate entry from the
    // pass above previously made the second `SocketSet::remove` for the
    // same handle panic on an already-removed socket.
    stack.poll();
}
