// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Fake-hardware, assembled-pipeline test for `Stack`.
//!
//! Every other `ward-net` smoltcp test exercises one piece of the pipeline
//! (DNS relay, TCP flow table, egress-check wiring, SSRF guard) in
//! isolation with the other pieces stubbed out. This test wires a single
//! `Stack` with a fake `Resolver`, a fake `Connector` backed by a real local
//! `TcpListener`, and a fake egress-check callback, then drives the whole
//! sequence end to end through the raw socketpair harness: ARP, a DNS query
//! for a domain, a guest SYN to the resolved address, the TCP handshake,
//! and bidirectional payload delivery. Proves the pieces interoperate, not
//! just that each works alone.

#![cfg(feature = "smoltcp")]

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DnsFlags, DnsOpcode, DnsPacket, DnsQueryType, DnsQuestion,
    DnsRepr, EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpAddress, IpProtocol,
    Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket, UdpRepr,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use ward_net::smoltcp_backend::{Connector, EgressCheckFn, Resolver, Stack};

/// MAC `Stack`'s interface already answers on (mirrors the private
/// `INTERFACE_HARDWARE_ADDR` constant in `smoltcp_backend`), so a guest
/// frame addressed here is accepted instead of dropped as a MAC mismatch.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface must own for a guest frame addressed
/// here (the ARP handshake and the DNS query below) to be accepted rather
/// than dropped.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Pre-DHCP guest address for this test; DHCP itself is a separate
/// scenario, so this is only ever used as a source address.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GUEST_DNS_SRC_PORT: u16 = 54321;
const GUEST_TCP_SRC_PORT: u16 = 55555;
const DNS_SERVER_PORT: u16 = 53;
const QUERY_TRANSACTION_ID: u16 = 0xbeef;
/// Domain this scenario's guest resolves and then connects to; the fake
/// egress-check callback only answers `true` for this domain, mirroring a
/// real allowlist that matched it.
const DOMAIN: &str = "pipeline.svc.ward.test";
/// A real but unrelated public IP used only as an address label the guest's
/// SYN targets; never actually dialed, since the fake `Connector` below
/// redirects every connect attempt to a real loopback listener this test
/// controls.
const RESOLVED_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
const RESOLVED_PORT: u16 = 443;
/// The common cloud metadata service address, a link-local address that
/// must never be reachable from a guest regardless of what the egress check
/// would have said. Mirrors `smoltcp_ssrf_guard.rs`'s `METADATA_ADDR`
/// constant of the same value.
const METADATA_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
const METADATA_PORT: u16 = 80;

/// Fake `Resolver` answering only `DOMAIN`, so a resolved answer in this
/// test can only have come from this injected resolver, never a real
/// lookup.
struct FakeResolver;

#[async_trait::async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        if name == DOMAIN {
            vec![IpAddr::V4(RESOLVED_ADDR)]
        } else {
            Vec::new()
        }
    }
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

/// Fake `Connector` that counts invocations rather than connecting anywhere,
/// so the metadata-address scenario below can assert the connect stage was
/// never reached.
struct CountingConnector {
    call_count: Arc<Mutex<u32>>,
}

#[async_trait::async_trait]
impl Connector for CountingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        *self.call_count.lock().unwrap() += 1;
        Err(io::Error::other(
            "CountingConnector should never be dialed for a flow to the metadata address",
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

/// Encodes `name` as wire-format DNS labels (length-prefixed, zero
/// terminated), mirroring how `smoltcp::socket::dns::Socket::start_query`
/// builds a raw name from a human-friendly one.
fn encode_dns_name(name: &str) -> Vec<u8> {
    let mut encoded = Vec::new();
    for label in name.split('.') {
        encoded.push(label.len() as u8);
        encoded.extend_from_slice(label.as_bytes());
    }
    encoded.push(0);
    encoded
}

/// Builds a complete Ethernet+IPv4+UDP+DNS query frame a guest would send
/// to look up `domain`, with correctly computed IPv4/UDP checksums so the
/// checksum validation smoltcp runs by default on ingress accepts it.
/// Mirrors `smoltcp_dns.rs`'s helper of the same shape.
fn build_dns_query_frame(domain: &str) -> Vec<u8> {
    let raw_name = encode_dns_name(domain);
    let dns_repr = DnsRepr {
        transaction_id: QUERY_TRANSACTION_ID,
        opcode: DnsOpcode::Query,
        flags: DnsFlags::RECURSION_DESIRED,
        question: DnsQuestion {
            name: &raw_name,
            type_: DnsQueryType::A,
        },
    };
    let dns_len = dns_repr.buffer_len();

    let udp_repr = UdpRepr {
        src_port: GUEST_DNS_SRC_PORT,
        dst_port: DNS_SERVER_PORT,
    };
    let ip_payload_len = udp_repr.header_len() + dns_len;

    let ip_repr = Ipv4Repr {
        src_addr: GUEST_ADDR,
        dst_addr: GATEWAY_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: ip_payload_len,
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: GATEWAY_MAC,
        ethertype: EthernetProtocol::Ipv4,
    };

    let total_len = eth_repr.buffer_len() + ip_repr.buffer_len() + ip_payload_len;
    let mut buf = vec![0u8; total_len];

    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth_frame);

    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());

    let mut udp_packet = UdpPacket::new_unchecked(ip_packet.payload_mut());
    udp_repr.emit(
        &mut udp_packet,
        &IpAddress::Ipv4(GUEST_ADDR),
        &IpAddress::Ipv4(GATEWAY_ADDR),
        dns_len,
        |dns_buf| dns_repr.emit(&mut DnsPacket::new_unchecked(dns_buf)),
        &ChecksumCapabilities::default(),
    );

    buf
}

/// Polls `stack` and checks `guest_fd` for a relayed response, retrying on
/// a short cadence until `budget` elapses, to tolerate a relay that
/// resolves the query asynchronously across more than one poll tick.
/// Mirrors `smoltcp_dns.rs`'s helper of the same shape.
async fn poll_until_response(
    stack: &mut Stack,
    guest_fd: &OwnedFd,
    budget: Duration,
) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        if let Some(frame) = try_read_frame(guest_fd) {
            return Some(frame);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// Builds a complete Ethernet+IPv4+TCP SYN frame a guest would send to open
/// a connection to `dst_addr:dst_port`. Mirrors `smoltcp_egress.rs`'s
/// `build_tcp_syn_frame_to` helper of the same shape.
fn build_tcp_syn_frame_to(dst_addr: Ipv4Addr, dst_port: u16) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port: GUEST_TCP_SRC_PORT,
        dst_port,
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
        dst_addr,
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
        &IpAddress::Ipv4(dst_addr),
        &ChecksumCapabilities::default(),
    );

    buf
}

/// Builds a plain (non-SYN) Ethernet+IPv4+TCP frame from the guest to
/// `dst_addr:dst_port`, acknowledging `ack` and carrying `payload`. Used
/// both for the ACK that completes the handshake (empty payload) and for
/// the data segment sent afterwards. Mirrors `smoltcp_flow.rs`'s
/// `build_tcp_ack_frame` helper of the same shape.
fn build_tcp_ack_frame(
    dst_addr: Ipv4Addr,
    dst_port: u16,
    seq: TcpSeqNumber,
    ack: TcpSeqNumber,
    payload: &[u8],
) -> Vec<u8> {
    let tcp_repr = TcpRepr {
        src_port: GUEST_TCP_SRC_PORT,
        dst_port,
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
        dst_addr,
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
        &IpAddress::Ipv4(dst_addr),
        &ChecksumCapabilities::default(),
    );

    buf
}

/// TCP header fields parsed out of a reply frame the stack sent back to the
/// guest, the minimum this scenario needs to react to the handshake and
/// later payload delivery. Mirrors `smoltcp_flow.rs`'s `ParsedTcpSegment`.
struct ParsedTcpSegment {
    control: TcpControl,
    seq_number: TcpSeqNumber,
    payload: Vec<u8>,
}

/// Parses `frame` as an Ethernet+IPv4+TCP frame, panicking if it isn't one:
/// a malformed frame here is a test bug, not something worth handling
/// gracefully. Mirrors `smoltcp_flow.rs`'s helper of the same shape.
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

/// Polls `stack` until a reply frame carrying the SYN control flag appears
/// on `guest_fd`, or `budget` elapses. Mirrors `smoltcp_flow.rs`'s helper of
/// the same shape.
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

/// Polls `stack` until a reply frame carrying the RST control flag appears
/// on `guest_fd`, or `budget` elapses. Mirrors `smoltcp_ssrf_guard.rs`'s
/// helper of the same shape.
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

/// Non-blocking read of whatever bytes are currently available on `stream`,
/// `None` if nothing has arrived yet. Mirrors `smoltcp_flow.rs`'s helper of
/// the same shape.
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
/// Mirrors `smoltcp_flow.rs`'s helper of the same shape.
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
/// Mirrors `smoltcp_flow.rs`'s helper of the same shape.
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
async fn given_fake_hardware_pipeline_when_guest_resolves_and_connects_then_flow_completes_end_to_end()
 {
    // Arrange: one Stack assembling every pipeline piece: a fake Resolver
    // that only answers DOMAIN, a fake egress-check callback that only
    // allows DOMAIN, and a fake Connector that redirects every connect
    // attempt to a real local TcpListener this test controls, so the host
    // side of the eventual connection is a real, readable/writable socket.
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
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver);
    let connector: Box<dyn Connector> = Box::new(LoopbackConnector { listener_addr });
    let egress_calls: Arc<Mutex<Vec<(String, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let egress_calls_for_closure = Arc::clone(&egress_calls);
    let egress_check: Box<EgressCheckFn> = Box::new(move |domain: &str, port: u16| {
        egress_calls_for_closure
            .lock()
            .unwrap()
            .push((domain.to_string(), port));
        let allowed = domain == DOMAIN;
        Box::pin(async move { allowed })
    });
    let mut stack = Stack::new(host_fd, resolver, connector, egress_check);

    // Act, step 1: ARP, so the stack's neighbor cache learns the guest's
    // MAC before any unicast reply needs routing back to it.
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act, step 2: the guest resolves DOMAIN via the stack's DNS relay.
    write_frame(&guest_fd, &build_dns_query_frame(DOMAIN));
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(RESOLVED_ADDR)),
        Some(DOMAIN),
        "resolved map should attribute the resolved address to the queried domain"
    );

    // Act, step 3: the guest opens a SYN to the resolved address, which
    // must clear the SSRF guard (a public address), the egress check (the
    // fake callback only allows DOMAIN), and then reach the fake Connector.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(RESOLVED_ADDR, RESOLVED_PORT),
    );
    let syn_ack = poll_until_syn_ack(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack's own tcp::Socket should reply with a SYN-ACK once the flow is admitted");

    let client_next_seq = TcpSeqNumber(1); // the guest's SYN consumed one sequence number
    let server_next_seq = syn_ack.seq_number + 1;
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(
            RESOLVED_ADDR,
            RESOLVED_PORT,
            client_next_seq,
            server_next_seq,
            &[],
        ),
    );
    stack.poll();
    tokio::time::sleep(Duration::from_millis(10)).await;
    stack.poll();

    let host_stream = tokio::time::timeout(Duration::from_secs(2), host_stream_rx)
        .await
        .expect("connector should have connected to the listener within the timeout")
        .expect("host stream sender should not be dropped before sending");

    // Act, step 4: the guest writes bytes over the now-established flow.
    let guest_payload = b"hello from guest";
    write_frame(
        &guest_fd,
        &build_tcp_ack_frame(
            RESOLVED_ADDR,
            RESOLVED_PORT,
            client_next_seq,
            server_next_seq,
            guest_payload,
        ),
    );

    // Assert, step 4: the host's real TcpStream receives exactly those
    // bytes.
    let received_by_host =
        poll_until_stream_bytes(&mut stack, &host_stream, Duration::from_secs(2))
            .await
            .expect("host should receive the guest's bytes once the flow is established");
    assert_eq!(
        received_by_host, guest_payload,
        "host side of the connection should receive exactly the bytes the guest sent"
    );

    // Act, step 5: the host writes bytes back over the same flow.
    let host_payload = b"hello from host";
    host_stream
        .try_write(host_payload)
        .expect("writing to a freshly accepted loopback stream should not block");

    // Assert, step 5: the guest receives exactly those bytes back through
    // the raw device.
    let received_by_guest = poll_until_guest_payload(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("guest should receive the host's bytes once the flow is established");
    assert_eq!(
        received_by_guest, host_payload,
        "guest side of the connection should receive exactly the bytes the host sent"
    );

    // Assert: the egress-check callback was consulted with the resolved
    // domain label (proving the DNS relay's resolved map fed the egress
    // check, not just the SSRF guard and connector on their own) and the
    // flow's destination port.
    assert_eq!(
        *egress_calls.lock().unwrap(),
        vec![(DOMAIN.to_string(), RESOLVED_PORT)],
        "egress-check callback should be called once with the domain resolved earlier \
         in the pipeline and the flow's destination port"
    );
}

#[tokio::test]
async fn given_fake_hardware_pipeline_when_guest_targets_metadata_address_then_flow_rejected() {
    // Arrange: a Stack with a fake egress-check callback that always
    // returns true, mirroring EgressProxy::check's real Open-mode behavior,
    // so the only thing standing between the guest and the metadata address
    // is the SSRF guard.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver);
    let connector_calls: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let connector: Box<dyn Connector> = Box::new(CountingConnector {
        call_count: Arc::clone(&connector_calls),
    });
    let egress_calls: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let egress_calls_for_closure = Arc::clone(&egress_calls);
    let egress_check: Box<EgressCheckFn> = Box::new(move |_domain: &str, _port: u16| {
        *egress_calls_for_closure.lock().unwrap() += 1;
        Box::pin(async { true })
    });
    let mut stack = Stack::new(host_fd, resolver, connector, egress_check);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: the guest sends a SYN directly to the cloud metadata address,
    // with no DNS lookup involved.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(METADATA_ADDR, METADATA_PORT),
    );

    // Assert: the guest receives an RST, and the egress-check callback is
    // never invoked, even though it would have allowed the flow, proving
    // the guard runs unconditionally and before the egress check gets a
    // chance to weigh in rather than being satisfied by an "allow" answer.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "guest should receive an RST for a SYN to the metadata address"
    );
    assert_eq!(
        *egress_calls.lock().unwrap(),
        0,
        "egress-check callback should never be invoked for a flow to the metadata address"
    );
    assert_eq!(
        *connector_calls.lock().unwrap(),
        0,
        "connector should never be dispatched for a flow to the metadata address"
    );
}
