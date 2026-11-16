// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! TCP flow table tests for `Stack`.
//!
//! Drives a guest-side TCP SYN through the raw socketpair harness and
//! asserts `Stack` opens the outbound connection via an injected
//! `Connector` rather than dialing the network itself, so a test never
//! needs live network access to prove the flow table's addressing logic.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
    TcpSeqNumber,
};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use ward_net::smoltcp_backend::{Connector, MAX_PENDING_CONNECTS, Resolver, Stack};

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

/// `Stack::new` requires an egress-check callback; no scenario in this file
/// exercises rejection, so this always allows rather than standing in for a
/// real allowlist policy.
fn always_allow_egress(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { true })
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
    build_tcp_syn_frame_from(GUEST_SRC_PORT)
}

/// Same as [`build_tcp_syn_frame`] with an explicit guest source port, so a
/// test can open many distinct flows.
fn build_tcp_syn_frame_from(src_port: u16) -> Vec<u8> {
    build_tcp_syn_with_seq(src_port, TcpSeqNumber(0))
}

/// Same as [`build_tcp_syn_frame_from`] with an explicit initial sequence number.
fn build_tcp_syn_with_seq(src_port: u16, seq_number: TcpSeqNumber) -> Vec<u8> {
    build_tcp_syn_custom(src_port, DEST_PORT, seq_number, GATEWAY_MAC, None)
}

/// A SYN with every field a test may need to vary: the destination port, the
/// Ethernet destination MAC and the MSS option.
fn build_tcp_syn_custom(
    src_port: u16,
    dst_port: u16,
    seq_number: TcpSeqNumber,
    dst_mac: EthernetAddress,
    max_seg_size: Option<u16>,
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port,
        dst_port,
        control: TcpControl::Syn,
        seq_number,
        ack_number: None,
        window_len: 65535,
        window_scale: None,
        max_seg_size,
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
        dst_addr: dst_mac,
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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
#[derive(Debug, Clone)]
struct ParsedTcpSegment {
    dst_port: u16,
    control: TcpControl,
    seq_number: TcpSeqNumber,
    ack_number: Option<TcpSeqNumber>,
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
        dst_port: tcp_repr.dst_port,
        control: tcp_repr.control,
        seq_number: tcp_repr.seq_number,
        ack_number: tcp_repr.ack_number,
        payload: tcp_repr.payload.to_vec(),
    }
}

/// Builds a plain (non-SYN) Ethernet+IPv4+TCP frame from the guest,
/// acknowledging `ack` and carrying `payload`. Used both for the ACK that
/// completes the handshake (empty payload) and for the data segment sent
/// afterwards.
fn build_tcp_ack_frame(seq: TcpSeqNumber, ack: TcpSeqNumber, payload: &[u8]) -> Vec<u8> {
    build_tcp_frame(TcpControl::None, seq, ack, payload)
}

/// Same as [`build_tcp_ack_frame`] from an explicit guest source port.
fn build_tcp_ack_frame_from(
    src_port: u16,
    seq: TcpSeqNumber,
    ack: TcpSeqNumber,
    payload: &[u8],
) -> Vec<u8> {
    build_tcp_frame_from(src_port, TcpControl::None, seq, ack, payload)
}

/// Same as [`build_tcp_ack_frame`] with an explicit control flag.
fn build_tcp_frame(
    control: TcpControl,
    seq: TcpSeqNumber,
    ack: TcpSeqNumber,
    payload: &[u8],
) -> Vec<u8> {
    build_tcp_frame_from(GUEST_SRC_PORT, control, seq, ack, payload)
}

fn build_tcp_frame_from(
    src_port: u16,
    control: TcpControl,
    seq: TcpSeqNumber,
    ack: TcpSeqNumber,
    payload: &[u8],
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port,
        dst_port: DEST_PORT,
        control,
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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

async fn poll_until_fin(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Option<ParsedTcpSegment> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        while let Some(frame) = try_read_frame(guest_fd) {
            let segment = parse_tcp_segment(&frame);
            if segment.control == TcpControl::Fin {
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
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

    // poll_until_fin polls in a loop until the flow starts closing (the
    // write channel's Closed arm and the read channel's Disconnected arm
    // both fire once the io task ends, and must not queue the flow for
    // removal twice).
    let fin = poll_until_fin(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        fin.is_some(),
        "the guest should get a graceful FIN, not an RST, once the host closes its side"
    );

    // Assert: one more poll must not panic. This is where
    // `sockets_pending_removal` gets drained; a duplicate entry from the
    // pass above previously made the second `SocketSet::remove` for the
    // same handle panic on an already-removed socket.
    stack.poll();
}

#[tokio::test]
async fn given_host_sends_then_closes_when_polled_then_guest_gets_data_before_fin_and_no_rst() {
    // Arrange: an established flow whose host peer writes a response and
    // closes straight away, like an HTTP/1.0 server.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind loopback listener");
    let listener_addr = listener.local_addr().expect("local addr");
    let (host_stream_tx, host_stream_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (host_stream, _) = listener.accept().await.expect("accept");
        let _ = host_stream_tx.send(host_stream);
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("SYN-ACK");
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(TcpSeqNumber(1), syn_ack.seq_number + 1, &[]),
    );
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();
    let mut host_stream = tokio::time::timeout(Duration::from_secs(2), host_stream_rx)
        .await
        .expect("host connect")
        .expect("host stream");

    // Act: write the response and close; collect what the guest sees.
    tokio::io::AsyncWriteExt::write_all(&mut host_stream, b"response body")
        .await
        .expect("host write");
    drop(host_stream);
    let mut received = Vec::new();
    let mut saw_fin = false;
    let mut saw_rst = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !saw_fin {
        stack.poll();
        while let Some(frame) = try_read_frame(&guest_fd) {
            let segment = parse_tcp_segment(&frame);
            received.extend_from_slice(&segment.payload);
            saw_fin |= segment.control == TcpControl::Fin;
            saw_rst |= segment.control == TcpControl::Rst;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Assert
    assert!(!saw_rst, "the guest must not be reset");
    assert!(saw_fin, "the guest should get a FIN once the host closes");
    assert_eq!(received, b"response body");
}

/// An established flow whose host peer is the returned stream.
struct EstablishedFlow {
    stack: Stack,
    guest_fd: OwnedFd,
    host_stream: TcpStream,
    /// Next sequence number the guest sends.
    guest_seq: TcpSeqNumber,
    /// Next sequence number ward sends.
    server_seq: TcpSeqNumber,
}

async fn establish_flow() -> EstablishedFlow {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind loopback listener");
    let listener_addr = listener.local_addr().expect("local addr");
    let (host_stream_tx, host_stream_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (host_stream, _) = listener.accept().await.expect("accept");
        let _ = host_stream_tx.send(host_stream);
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("SYN-ACK");
    let guest_seq = TcpSeqNumber(1);
    let server_seq = syn_ack.seq_number + 1;
    write_frame(&guest_fd, &build_tcp_ack_frame(guest_seq, server_seq, &[]));
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();
    let host_stream = tokio::time::timeout(Duration::from_secs(2), host_stream_rx)
        .await
        .expect("host connect")
        .expect("host stream");
    EstablishedFlow {
        stack,
        guest_fd,
        host_stream,
        guest_seq,
        server_seq,
    }
}

/// Polls until the flow table is empty or `budget` runs out.
async fn poll_until_no_flows(stack: &mut Stack, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        if stack.flow_count_for_test() == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn given_host_closed_and_guest_never_closes_when_half_close_deadline_passes_then_flow_is_removed()
 {
    // Arrange: ward sends its FIN and the guest stays silent, like an idle
    // keep-alive connection.
    let mut flow = establish_flow().await;
    flow.stack
        .set_half_close_timeout_for_test(Duration::from_millis(100));
    drop(flow.host_stream);
    let fin = poll_until_fin(&mut flow.stack, &flow.guest_fd, Duration::from_secs(2)).await;
    assert!(fin.is_some(), "ward should send its FIN first");
    assert_eq!(
        flow.stack.flow_count_for_test(),
        1,
        "flow waits for the guest"
    );

    // Act / Assert: the deadline frees the slot.
    assert!(
        poll_until_no_flows(&mut flow.stack, Duration::from_secs(3)).await,
        "a half-closed flow must not live forever"
    );
}

#[tokio::test]
async fn given_established_flow_when_guest_resets_then_flow_is_removed() {
    // Arrange: a host chunk is still undelivered when the guest resets.
    let mut flow = establish_flow().await;

    // Act
    write_frame(
        &flow.guest_fd,
        &build_tcp_frame(TcpControl::Rst, flow.guest_seq, flow.server_seq, &[]),
    );

    // Assert
    assert!(
        poll_until_no_flows(&mut flow.stack, Duration::from_secs(3)).await,
        "a reset flow must leave the table"
    );
    drop(flow.host_stream);
}

/// Makes dropping `stream` send an RST instead of a FIN, so the stack's host
/// read fails with a connection reset.
fn set_zero_linger(stream: &TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: the fd is a valid open socket and the pointer and length
    // describe a valid `linger` value.
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(ret, 0, "setsockopt(SO_LINGER) failed");
}

#[tokio::test]
async fn given_host_resets_connection_when_polled_then_guest_gets_rst_not_fin() {
    // Arrange
    let mut flow = establish_flow().await;
    set_zero_linger(&flow.host_stream);

    // Act: the server aborts mid-response.
    drop(flow.host_stream);
    let mut saw_fin = false;
    let mut saw_rst = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !saw_rst {
        flow.stack.poll();
        while let Some(frame) = try_read_frame(&flow.guest_fd) {
            let control = parse_tcp_segment(&frame).control;
            saw_fin |= control == TcpControl::Fin;
            saw_rst |= control == TcpControl::Rst;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Assert
    assert!(saw_rst, "a host read error must reset the guest");
    assert!(!saw_fin, "a host read error must not look like a clean end");
}

#[tokio::test]
async fn given_idle_established_flow_when_idle_timeout_passes_then_guest_is_reset() {
    // Arrange
    let mut flow = establish_flow().await;
    flow.stack
        .set_idle_timeout_for_test(Duration::from_millis(100));

    // Act / Assert
    let rst = poll_until_rst(&mut flow.stack, &flow.guest_fd, Duration::from_secs(3)).await;
    assert!(rst.is_some(), "an idle flow must be reset");
    assert!(poll_until_no_flows(&mut flow.stack, Duration::from_secs(3)).await);
    drop(flow.host_stream);
}

#[tokio::test]
async fn given_flow_with_traffic_when_idle_timeout_is_short_then_activity_keeps_it_alive() {
    // Arrange
    let mut flow = establish_flow().await;
    let idle = Duration::from_secs(1);
    flow.stack.set_idle_timeout_for_test(idle);

    // Act: host bytes arrive more often than the idle timeout, for longer
    // than the idle timeout in total.
    let start = std::time::Instant::now();
    while start.elapsed() < idle * 2 {
        tokio::io::AsyncWriteExt::write_all(&mut flow.host_stream, b"x")
            .await
            .expect("host write");
        for _ in 0..5 {
            flow.stack.poll();
            while try_read_frame(&flow.guest_fd).is_some() {}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Assert
    assert_eq!(
        flow.stack.flow_count_for_test(),
        1,
        "activity resets the timer"
    );
}

/// Bytes the host sends in the stalled and slow-drain tests: more than the
/// stack's per-flow send buffer, so the flow cannot finish by itself.
const BULK_HOST_BYTES: usize = 128 * 1024;

/// Reads every frame waiting for the guest, queues the sequence number just
/// past each data segment in `ends`, and reports whether an RST arrived.
fn drain_frames(guest_fd: &OwnedFd, ends: &mut VecDeque<TcpSeqNumber>) -> bool {
    let mut saw_rst = false;
    while let Some(frame) = try_read_frame(guest_fd) {
        let segment = parse_tcp_segment(&frame);
        saw_rst |= segment.control == TcpControl::Rst;
        if !segment.payload.is_empty() {
            ends.push_back(segment.seq_number + segment.payload.len());
        }
    }
    saw_rst
}

#[tokio::test]
async fn given_guest_never_drains_when_idle_timeout_passes_then_flow_is_reset_with_bytes_queued() {
    // Arrange: the host has more bytes queued than the guest ever takes.
    let mut flow = establish_flow().await;
    flow.stack
        .set_idle_timeout_for_test(Duration::from_millis(300));
    tokio::io::AsyncWriteExt::write_all(&mut flow.host_stream, &vec![7u8; BULK_HOST_BYTES])
        .await
        .expect("host write");

    // Act / Assert: the guest sends no ACK, so no progress is made.
    let rst = poll_until_rst(&mut flow.stack, &flow.guest_fd, Duration::from_secs(5)).await;
    assert!(rst.is_some(), "a flow with no progress must be reset");
}

#[tokio::test]
async fn given_guest_drains_slowly_when_idle_timeout_is_longer_than_each_step_then_flow_survives() {
    // Arrange
    let mut flow = establish_flow().await;
    let idle = Duration::from_secs(1);
    flow.stack.set_idle_timeout_for_test(idle);
    tokio::io::AsyncWriteExt::write_all(&mut flow.host_stream, &vec![7u8; BULK_HOST_BYTES])
        .await
        .expect("host write");

    // Act: the guest acknowledges one segment about every 100 ms, for
    // longer than the idle timeout in total, so the host's bytes outlast it.
    let start = std::time::Instant::now();
    let mut ends = VecDeque::new();
    let mut saw_rst = false;
    while start.elapsed() < idle * 2 {
        flow.stack.poll();
        saw_rst |= drain_frames(&flow.guest_fd, &mut ends);
        if let Some(end) = ends.pop_front() {
            write_frame(
                &flow.guest_fd,
                &build_tcp_ack_frame(flow.guest_seq, end, &[]),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Assert
    assert!(!saw_rst, "a draining guest must not be reset");
    assert_eq!(flow.stack.flow_count_for_test(), 1);
}

#[tokio::test]
async fn given_host_reset_while_guest_uploads_when_polled_then_guest_gets_rst_not_fin() {
    // Arrange: the host aborts the connection while the guest is sending.
    let mut flow = establish_flow().await;
    set_zero_linger(&flow.host_stream);
    drop(flow.host_stream);
    write_frame(
        &flow.guest_fd,
        &build_tcp_ack_frame(flow.guest_seq, flow.server_seq, b"upload"),
    );

    // Act
    let mut saw_fin = false;
    let mut saw_rst = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline && !saw_rst {
        flow.stack.poll();
        while let Some(frame) = try_read_frame(&flow.guest_fd) {
            let control = parse_tcp_segment(&frame).control;
            saw_fin |= control == TcpControl::Fin;
            saw_rst |= control == TcpControl::Rst;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Assert
    assert!(saw_rst, "a cut upload must reset the guest");
    assert!(!saw_fin);
}

/// Fake `Connector` that fails every connect with the given error kind,
/// standing in for a closed host port.
struct FailingConnector(io::ErrorKind);

#[async_trait::async_trait]
impl Connector for FailingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        Err(io::Error::from(self.0))
    }
}

fn deny_all_egress(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { false })
}

/// Polls `stack` for `budget` and returns every TCP segment the guest received.
async fn drain_guest(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Vec<ParsedTcpSegment> {
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        while let Some(frame) = try_read_frame(guest_fd) {
            if EthernetFrame::new_checked(&frame)
                .map(|f| f.ethertype() == EthernetProtocol::Ipv4)
                .unwrap_or(false)
            {
                seen.push(parse_tcp_segment(&frame));
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    seen
}

/// Control flags of what [`drain_guest`] saw.
async fn collect_guest_controls(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Vec<TcpControl> {
    let segments = drain_guest(stack, guest_fd, budget).await;
    segments.iter().map(|segment| segment.control).collect()
}

#[tokio::test]
async fn given_egress_denied_when_guest_syns_then_guest_gets_rst_and_never_a_syn_ack() {
    let (guest_fd, host_fd) = socketpair_dgram();
    let called_with = Arc::new(Mutex::new(None));
    let connector: Box<dyn Connector> = Box::new(RecordingConnector {
        called_with: Arc::clone(&called_with),
    });
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(deny_all_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(&guest_fd, &build_tcp_syn_frame());
    let seen = collect_guest_controls(&mut stack, &guest_fd, Duration::from_millis(500)).await;

    assert!(
        !seen.contains(&TcpControl::Syn),
        "a denied destination must never be accepted, saw {seen:?}"
    );
    assert!(
        seen.contains(&TcpControl::Rst),
        "guest should be reset, saw {seen:?}"
    );
    assert!(
        called_with.lock().unwrap().is_none(),
        "the connector must not run for a denied destination"
    );
}

#[tokio::test]
async fn given_host_port_refused_when_guest_syns_then_guest_gets_rst_and_never_a_syn_ack() {
    let (guest_fd, host_fd) = socketpair_dgram();
    let connector: Box<dyn Connector> =
        Box::new(FailingConnector(io::ErrorKind::ConnectionRefused));
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(&guest_fd, &build_tcp_syn_frame());
    let seen = collect_guest_controls(&mut stack, &guest_fd, Duration::from_millis(500)).await;

    assert!(
        !seen.contains(&TcpControl::Syn),
        "a refused host connect must never be accepted, saw {seen:?}"
    );
    assert!(
        seen.contains(&TcpControl::Rst),
        "guest should be reset, saw {seen:?}"
    );
    assert_eq!(stack.flow_count_for_test(), 0);
    assert_eq!(
        stack.pending_connect_count_for_test(),
        0,
        "a failed connect must free its slot"
    );
}

/// Fake `Connector` that waits for a release signal before dialing the
/// loopback listener, so a test controls when the host connect finishes.
struct GatedConnector {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    listener_addr: SocketAddr,
}

#[async_trait::async_trait]
impl Connector for GatedConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        let gate = self.gate.lock().unwrap().take().expect("connect runs once");
        let _ = gate.await;
        TcpStream::connect(self.listener_addr).await
    }
}

#[tokio::test]
async fn given_allowed_destination_when_connect_is_slow_then_syn_ack_waits_for_the_connect_and_data_flows()
 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_addr = listener.local_addr().unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let connector: Box<dyn Connector> = Box::new(GatedConnector {
        gate: Mutex::new(Some(release_rx)),
        listener_addr,
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(&guest_fd, &build_tcp_syn_frame());
    let early = collect_guest_controls(&mut stack, &guest_fd, Duration::from_millis(300)).await;
    assert!(
        early.is_empty(),
        "no reply before the host connect finishes, saw {early:?}"
    );
    assert_eq!(stack.pending_connect_count_for_test(), 1);

    release_tx.send(()).unwrap();
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("SYN-ACK once the host connect has succeeded");
    let (host_stream, _) = listener.accept().await.unwrap();

    let client_next_seq = TcpSeqNumber(1);
    let server_next_seq = syn_ack.seq_number + 1;
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(client_next_seq, server_next_seq, &[]),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(client_next_seq, server_next_seq, b"ping"),
    );
    let got = poll_until_stream_bytes(&mut stack, &host_stream, Duration::from_secs(2)).await;
    assert_eq!(got.as_deref(), Some(&b"ping"[..]));
    host_stream.try_write(b"pong").unwrap();
    let reply = poll_until_guest_payload(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert_eq!(reply.as_deref(), Some(&b"pong"[..]));
}

#[tokio::test]
async fn given_syn_burst_when_connects_hang_then_pending_connects_are_capped_and_excess_syns_dropped()
 {
    let (guest_fd, host_fd) = socketpair_dgram();
    let connector: Box<dyn Connector> = Box::new(HangingConnector);
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // A small datagram socket buffer holds only a few SYNs at once, so the
    // burst is fed in short batches with a poll between them.
    let mut seen = Vec::new();
    for batch in 0..((MAX_PENDING_CONNECTS + 40) / 8) {
        for i in 0..8 {
            write_frame(
                &guest_fd,
                &build_tcp_syn_frame_from(30_000 + (batch * 8 + i) as u16),
            );
        }
        seen.extend(collect_guest_controls(&mut stack, &guest_fd, Duration::from_millis(20)).await);
    }

    assert_eq!(stack.pending_connect_count_for_test(), MAX_PENDING_CONNECTS);
    assert!(
        seen.is_empty(),
        "held and dropped SYNs get no reply, saw {seen:?}"
    );
}

/// Fake `Connector` that answers each call, in call order, with the next
/// queued listener address, or a refusal for `None`. Calls start in SYN order
/// on a current-thread runtime, so a test knows which flow gets which answer.
struct SequenceConnector {
    answers: Mutex<VecDeque<Option<SocketAddr>>>,
}

#[async_trait::async_trait]
impl Connector for SequenceConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("queued answer");
        match answer {
            Some(addr) => TcpStream::connect(addr).await,
            None => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        }
    }
}

/// Port of the second flow in the same-destination tests.
const OTHER_SRC_PORT: u16 = 40_001;

/// Sends two SYNs to the same destination (`OTHER_SRC_PORT` first, then
/// `GUEST_SRC_PORT`) and polls until both connects have finished.
async fn start_two_flows(stack: &mut Stack, guest_fd: &OwnedFd) -> Vec<ParsedTcpSegment> {
    write_frame(guest_fd, &build_tcp_syn_frame_from(OTHER_SRC_PORT));
    write_frame(guest_fd, &build_tcp_syn_frame());
    stack.poll();
    assert_eq!(stack.pending_connect_count_for_test(), 2);
    // Lets both connect tasks finish before the next poll sees them.
    tokio::time::sleep(Duration::from_millis(100)).await;
    drain_guest(stack, guest_fd, Duration::from_millis(300)).await
}

/// Finds the SYN-ACK sent to `port`, if any.
fn syn_ack_for(segments: &[ParsedTcpSegment], port: u16) -> Option<&ParsedTcpSegment> {
    segments
        .iter()
        .find(|segment| segment.dst_port == port && segment.control == TcpControl::Syn)
}

#[tokio::test]
async fn given_two_flows_to_one_destination_when_one_fails_then_it_is_reset_and_never_bridged() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([None, Some(listener.local_addr().unwrap())])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let mut seen = start_two_flows(&mut stack, &guest_fd).await;
    let syn_ack = syn_ack_for(&seen, GUEST_SRC_PORT)
        .expect("the good flow is accepted")
        .clone();
    let (host_stream, _) = listener.accept().await.unwrap();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(TcpSeqNumber(1), syn_ack.seq_number + 1, &[]),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(TcpSeqNumber(1), syn_ack.seq_number + 1, b"from good"),
    );
    let got = poll_until_stream_bytes(&mut stack, &host_stream, Duration::from_secs(2)).await;
    assert_eq!(got.as_deref(), Some(&b"from good"[..]));
    host_stream.try_write(b"to good").unwrap();
    seen.extend(drain_guest(&mut stack, &guest_fd, Duration::from_millis(300)).await);

    let failed: Vec<_> = seen
        .iter()
        .filter(|s| s.dst_port == OTHER_SRC_PORT)
        .collect();
    assert!(
        failed.iter().any(|s| s.control == TcpControl::Rst),
        "failed flow is reset: {failed:?}"
    );
    assert!(
        failed
            .iter()
            .all(|s| s.control != TcpControl::Syn && s.payload.is_empty()),
        "failed flow never gets an accept or data: {failed:?}"
    );
    assert!(
        seen.iter()
            .any(|s| s.dst_port == GUEST_SRC_PORT && s.payload == b"to good"),
        "the good flow still receives its own host stream's data"
    );
}

#[tokio::test]
async fn given_two_flows_to_one_destination_when_both_connect_then_each_is_bridged_to_its_own_stream()
 {
    let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([
            Some(listener_a.local_addr().unwrap()),
            Some(listener_b.local_addr().unwrap()),
        ])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let mut seen = start_two_flows(&mut stack, &guest_fd).await;
    // A flow to the same destination waits one poll for the other's replay.
    seen.extend(drain_guest(&mut stack, &guest_fd, Duration::from_millis(200)).await);
    let (stream_a, _) = listener_a.accept().await.unwrap();
    let (stream_b, _) = listener_b.accept().await.unwrap();
    let flows = [
        (OTHER_SRC_PORT, &stream_a, b"alpha"),
        (GUEST_SRC_PORT, &stream_b, b"bravo"),
    ];
    for (port, _, payload) in flows {
        let syn_ack = syn_ack_for(&seen, port).expect("both flows are accepted");
        let ack = syn_ack.seq_number + 1;
        write_frame(
            &guest_fd,
            &build_tcp_ack_frame_from(port, TcpSeqNumber(1), ack, &[]),
        );
        stack.poll();
        write_frame(
            &guest_fd,
            &build_tcp_ack_frame_from(port, TcpSeqNumber(1), ack, payload),
        );
    }
    for (_, stream, payload) in flows {
        let got = poll_until_stream_bytes(&mut stack, stream, Duration::from_secs(2)).await;
        assert_eq!(
            got.as_deref(),
            Some(&payload[..]),
            "each stream gets only its own flow's bytes"
        );
    }
    stream_a.try_write(b"to-a").unwrap();
    stream_b.try_write(b"to-b").unwrap();
    let replies = drain_guest(&mut stack, &guest_fd, Duration::from_millis(300)).await;
    for (port, want) in [
        (OTHER_SRC_PORT, &b"to-a"[..]),
        (GUEST_SRC_PORT, &b"to-b"[..]),
    ] {
        assert!(
            replies
                .iter()
                .any(|s| s.dst_port == port && s.payload == want),
            "flow on port {port} receives its own host stream's bytes"
        );
    }
}

/// Fake `Connector` that counts its calls and never finishes.
struct CountingHangingConnector(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl Connector for CountingHangingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test]
async fn given_pending_connect_when_guest_retransmits_syn_then_it_is_dropped_without_a_second_connect()
 {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connector: Box<dyn Connector> = Box::new(CountingHangingConnector(Arc::clone(&calls)));
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(&guest_fd, &build_tcp_syn_frame());
    stack.poll();
    write_frame(&guest_fd, &build_tcp_syn_frame());
    let seen = collect_guest_controls(&mut stack, &guest_fd, Duration::from_millis(200)).await;

    assert!(
        seen.is_empty(),
        "a held SYN and its retransmit get no reply, saw {seen:?}"
    );
    assert_eq!(stack.pending_connect_count_for_test(), 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn given_pending_connect_when_syn_is_retransmitted_with_new_isn_then_syn_ack_acks_the_replacement()
 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let (release_tx, release_rx) = oneshot::channel();
    let connector: Box<dyn Connector> = Box::new(GatedConnector {
        gate: Mutex::new(Some(release_rx)),
        listener_addr: listener.local_addr().unwrap(),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(
        &guest_fd,
        &build_tcp_syn_with_seq(GUEST_SRC_PORT, TcpSeqNumber(0)),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_syn_with_seq(GUEST_SRC_PORT, TcpSeqNumber(1000)),
    );
    stack.poll();
    release_tx.send(()).unwrap();
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("SYN-ACK once the connect succeeds");

    assert_eq!(syn_ack.ack_number, Some(TcpSeqNumber(1001)));
}

#[tokio::test]
async fn given_pending_connect_when_deadline_passes_then_guest_is_reset_and_slot_freed() {
    let connector: Box<dyn Connector> = Box::new(HangingConnector);
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    stack.set_connect_timeout_for_test(Duration::from_millis(100));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(&guest_fd, &build_tcp_syn_frame());
    let seen = drain_guest(&mut stack, &guest_fd, Duration::from_millis(600)).await;

    assert!(
        seen.iter().any(|s| s.control == TcpControl::Rst),
        "timed out connect is reset: {seen:?}"
    );
    assert!(
        seen.iter().all(|s| s.control != TcpControl::Syn),
        "and never accepted: {seen:?}"
    );
    assert_eq!(stack.pending_connect_count_for_test(), 0);
}

/// Fake `Connector` that flags when its future starts and when it is dropped.
struct DropFlagConnector {
    started: Arc<std::sync::atomic::AtomicBool>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Connector for DropFlagConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        let _guard = SetOnDrop(Arc::clone(&self.dropped));
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }
}

#[tokio::test]
async fn given_pending_connect_when_stack_is_dropped_then_connect_task_is_aborted() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let connector: Box<dyn Connector> = Box::new(DropFlagConnector {
        started: Arc::clone(&started),
        dropped: Arc::clone(&dropped),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;
    write_frame(&guest_fd, &build_tcp_syn_frame());
    stack.poll();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        "connect started"
    );
    assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));

    drop(stack);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the connect future must be dropped with the stack"
    );
}

/// A MAC that is not the interface's.
/// Offset of the TCP checksum in a SYN frame with no IP options: Ethernet 14,
/// IPv4 20, then 16 into the TCP header.
const MIN_TCP_CHECKSUM_OFFSET: usize = 14 + 20 + 16;

const OTHER_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x99]);

#[tokio::test]
async fn given_three_flows_ready_together_when_one_to_a_shared_destination_fails_then_it_never_reaches_the_other_listener()
 {
    let listener_g = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([
            Some(listener_g.local_addr().unwrap()),
            None,
            Some(listener_b.local_addr().unwrap()),
        ])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Keys sort G < F < B. G goes to another port, F and B share one.
    let other_port = DEST_PORT + 1;
    write_frame(
        &guest_fd,
        &build_tcp_syn_custom(40_000, other_port, TcpSeqNumber(0), GATEWAY_MAC, None),
    );
    write_frame(&guest_fd, &build_tcp_syn_frame_from(40_001));
    write_frame(&guest_fd, &build_tcp_syn_frame_from(40_002));
    stack.poll();
    assert_eq!(stack.pending_connect_count_for_test(), 3);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seen = drain_guest(&mut stack, &guest_fd, Duration::from_millis(400)).await;

    let failed: Vec<_> = seen.iter().filter(|s| s.dst_port == 40_001).collect();
    assert!(
        failed.iter().any(|s| s.control == TcpControl::Rst),
        "denied flow is reset: {failed:?}"
    );
    assert!(
        failed.iter().all(|s| s.control != TcpControl::Syn),
        "and never accepted: {failed:?}"
    );
    assert!(syn_ack_for(&seen, 40_000).is_some(), "flow G is accepted");
    let syn_ack = syn_ack_for(&seen, 40_002).expect("flow B is accepted");
    let ack = syn_ack.seq_number + 1;
    let (stream_b, _) = listener_b.accept().await.unwrap();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame_from(40_002, TcpSeqNumber(1), ack, &[]),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame_from(40_002, TcpSeqNumber(1), ack, b"from b"),
    );
    let got = poll_until_stream_bytes(&mut stack, &stream_b, Duration::from_secs(2)).await;
    assert_eq!(
        got.as_deref(),
        Some(&b"from b"[..]),
        "B reaches its own host stream"
    );
}

#[tokio::test]
async fn given_syn_to_another_mac_when_polled_then_no_connect_and_a_later_flow_still_connects() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([Some(listener.local_addr().unwrap())])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    write_frame(
        &guest_fd,
        &build_tcp_syn_custom(GUEST_SRC_PORT, DEST_PORT, TcpSeqNumber(0), OTHER_MAC, None),
    );
    let seen = drain_guest(&mut stack, &guest_fd, Duration::from_millis(200)).await;
    assert!(seen.is_empty(), "smoltcp drops it silently: {seen:?}");
    assert_eq!(
        stack.pending_connect_count_for_test(),
        0,
        "no egress check or connect starts"
    );

    write_frame(&guest_fd, &build_tcp_syn_frame_from(OTHER_SRC_PORT));
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        syn_ack.is_some(),
        "a later flow to the same destination connects"
    );
}

#[tokio::test]
async fn given_pending_connect_when_replacement_syn_has_a_bad_mac_then_the_original_is_kept_and_nothing_blocks()
 {
    let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([
            Some(listener_a.local_addr().unwrap()),
            Some(listener_b.local_addr().unwrap()),
        ])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // No await between the polls, so the connect task has not finished yet.
    write_frame(
        &guest_fd,
        &build_tcp_syn_with_seq(GUEST_SRC_PORT, TcpSeqNumber(0)),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_syn_custom(
            GUEST_SRC_PORT,
            DEST_PORT,
            TcpSeqNumber(500),
            OTHER_MAC,
            None,
        ),
    );
    stack.poll();
    let seen = drain_guest(&mut stack, &guest_fd, Duration::from_millis(300)).await;
    let syn_ack = syn_ack_for(&seen, GUEST_SRC_PORT).expect("the original SYN is answered");
    assert_eq!(syn_ack.ack_number, Some(TcpSeqNumber(1)));
    assert_eq!(stack.pending_connect_count_for_test(), 0);

    write_frame(&guest_fd, &build_tcp_syn_frame_from(OTHER_SRC_PORT));
    let later = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        later.is_some(),
        "a later flow to the same destination connects"
    );
}

/// Sends a SYN from `port`, waits for the SYN-ACK, then answers it with an RST
/// the way a guest that gave up on the connection does.
async fn syn_then_guest_reset(stack: &mut Stack, guest_fd: &OwnedFd, port: u16) {
    write_frame(guest_fd, &build_tcp_syn_frame_from(port));
    let syn_ack = poll_until_syn_ack(stack, guest_fd, Duration::from_secs(2))
        .await
        .expect("SYN-ACK once the host connect succeeds");
    let rst = build_tcp_frame_from(
        port,
        TcpControl::Rst,
        TcpSeqNumber(1),
        syn_ack.seq_number + 1,
        &[],
    );
    write_frame(guest_fd, &rst);
    drain_guest(stack, guest_fd, Duration::from_millis(50)).await;
}

#[tokio::test]
async fn given_guest_resets_during_handshake_when_polled_then_flow_is_gone_and_next_flow_gets_its_own_stream()
 {
    let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([
            Some(listener_a.local_addr().unwrap()),
            Some(listener_b.local_addr().unwrap()),
        ])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    syn_then_guest_reset(&mut stack, &guest_fd, GUEST_SRC_PORT).await;

    assert_eq!(
        stack.flow_count_for_test(),
        0,
        "a reset handshake frees its slot"
    );
    let (mut stream_a, _) = listener_a.accept().await.unwrap();
    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(1), stream_a.read(&mut buf)).await;
    assert_eq!(read.expect("host stream closed").unwrap(), 0);

    write_frame(&guest_fd, &build_tcp_syn_frame_from(OTHER_SRC_PORT));
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("the next flow is accepted");
    let ack = syn_ack.seq_number + 1;
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame_from(OTHER_SRC_PORT, TcpSeqNumber(1), ack, &[]),
    );
    stack.poll();
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame_from(OTHER_SRC_PORT, TcpSeqNumber(1), ack, b"second"),
    );
    let (stream_b, _) = listener_b.accept().await.unwrap();
    let got = poll_until_stream_bytes(&mut stack, &stream_b, Duration::from_secs(2)).await;
    assert_eq!(
        got.as_deref(),
        Some(&b"second"[..]),
        "bridged to its own stream"
    );
}

#[tokio::test]
async fn given_flow_reset_during_handshake_when_same_tuple_syns_again_then_a_fresh_connect_starts()
{
    let listener_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let listener_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let connector: Box<dyn Connector> = Box::new(SequenceConnector {
        answers: Mutex::new(VecDeque::from([
            Some(listener_a.local_addr().unwrap()),
            Some(listener_b.local_addr().unwrap()),
        ])),
    });
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    syn_then_guest_reset(&mut stack, &guest_fd, GUEST_SRC_PORT).await;
    write_frame(&guest_fd, &build_tcp_syn_frame_from(GUEST_SRC_PORT));
    let accepted = async {
        loop {
            stack.poll();
            tokio::time::sleep(Duration::from_millis(10)).await;
            if stack.flow_count_for_test() == 1 {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), accepted)
        .await
        .expect("a new flow is registered");
    let (_stream_b, _) = tokio::time::timeout(Duration::from_secs(1), listener_b.accept())
        .await
        .expect("the SYN started a second host connect")
        .unwrap();
}

#[tokio::test]
async fn given_syns_smoltcp_would_ignore_when_polled_then_only_the_valid_syn_starts_a_connect() {
    let connector: Box<dyn Connector> = Box::new(HangingConnector);
    let (guest_fd, host_fd) = socketpair_dgram();
    let mut stack = Stack::new(
        host_fd,
        Box::new(NullResolver),
        connector,
        Box::new(always_allow_egress),
    );
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let mut bad_checksum = build_tcp_syn_frame_from(40_010);
    let checksum_at = MIN_TCP_CHECKSUM_OFFSET;
    bad_checksum[checksum_at] ^= 0xFF;
    write_frame(&guest_fd, &bad_checksum);
    let port_zero = build_tcp_syn_custom(40_011, 0, TcpSeqNumber(0), GATEWAY_MAC, None);
    write_frame(&guest_fd, &port_zero);
    let zero_mss = build_tcp_syn_custom(40_012, DEST_PORT, TcpSeqNumber(0), GATEWAY_MAC, Some(0));
    write_frame(&guest_fd, &zero_mss);
    write_frame(&guest_fd, &build_tcp_syn_frame_from(OTHER_SRC_PORT));
    stack.poll();

    assert_eq!(
        stack.pending_connect_count_for_test(),
        1,
        "a bad checksum, destination port 0 and a zero MSS each start no connect"
    );
}
