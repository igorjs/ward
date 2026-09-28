// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! SSRF/DNS-rebinding guard tests for `Stack`.
//!
//! Drives a guest-side DNS query and a guest-side TCP SYN through the raw
//! socketpair harness and asserts `Stack` rejects a flow whose actual
//! destination is a private, loopback, or link-local address, even when
//! `resolved` associates that address with a domain label an allowlist
//! would otherwise trust.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DnsFlags, DnsOpcode, DnsPacket, DnsQueryType, DnsQuestion,
    DnsRepr, EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpAddress, IpProtocol,
    Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber, UdpPacket, UdpRepr,
};
use tokio::net::TcpStream;
use ward_net::smoltcp_backend::{Connector, Resolver, Stack};

/// MAC `Stack`'s interface already answers on (mirrors the private
/// `INTERFACE_HARDWARE_ADDR` constant in `smoltcp_backend`), so a guest
/// frame addressed here is accepted instead of dropped as a MAC mismatch.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface must own for a guest frame addressed
/// here (only used for the ARP handshake and DNS query below) to be
/// accepted rather than dropped.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Pre-DHCP guest address for this test; DHCP itself is a separate
/// scenario, so this is only ever used as a source address.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GUEST_DNS_SRC_PORT: u16 = 54321;
const GUEST_TCP_SRC_PORT: u16 = 55555;
const DNS_SERVER_PORT: u16 = 53;
const QUERY_TRANSACTION_ID: u16 = 0xbeef;
/// A domain name that looks like it could be on an allowlist; the attack
/// this scenario pins is a DNS answer for a plausible-looking domain that
/// actually points at the cloud metadata address below.
const EVIL_DOMAIN: &str = "evil.allowed.example";
/// The common cloud metadata service address, a link-local address that
/// must never be reachable from a guest regardless of what domain label,
/// if any, resolved to it.
const METADATA_ADDR: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
const METADATA_PORT: u16 = 80;
/// Destination port for the loopback/RFC1918 scenario below; the guard
/// classifies by destination IP only, so any port works here.
const PRIVATE_DEST_PORT: u16 = 80;
/// A real but unrelated public IP used only as an address label, never
/// actually dialed since the fake `Connector` intercepts every connect
/// attempt. Mirrors `smoltcp_flow.rs`'s `DEST_ADDR`; the guard under test
/// must let this destination through rather than block it.
const PUBLIC_DEST_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
const PUBLIC_DEST_PORT: u16 = 80;

/// Fake `Resolver` answering `EVIL_DOMAIN` with the metadata address,
/// standing in for an attacker who controls DNS answers for a
/// plausible-looking domain and points it at an address the guest should
/// never be allowed to reach.
struct MetadataAnsweringResolver;

#[async_trait::async_trait]
impl Resolver for MetadataAnsweringResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        if name == EVIL_DOMAIN {
            vec![IpAddr::V4(METADATA_ADDR)]
        } else {
            Vec::new()
        }
    }
}

/// `Stack::new` requires a `Resolver`, but the loopback/RFC1918 scenario
/// below never sends a DNS query, so this always answers empty rather than
/// standing in for a real lookup.
struct NullResolver;

#[async_trait::async_trait]
impl Resolver for NullResolver {
    async fn resolve(&self, _name: &str) -> Vec<IpAddr> {
        Vec::new()
    }
}

/// Fake `Connector` that records whether it was ever invoked, so a test can
/// assert a rejected flow never reached the connect stage at all.
struct RecordingConnector {
    called: Arc<Mutex<bool>>,
}

#[async_trait::async_trait]
impl Connector for RecordingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        *self.called.lock().unwrap() = true;
        Err(io::Error::other(
            "RecordingConnector should never be dialed for a flow to the metadata address",
        ))
    }
}

/// Fake `Connector` that records the address it was called with, so this
/// scenario can prove the guard let the flow through to the connect stage
/// rather than merely failing to error.
struct AddressRecordingConnector {
    called_with: Arc<Mutex<Option<SocketAddr>>>,
}

#[async_trait::async_trait]
impl Connector for AddressRecordingConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        *self.called_with.lock().unwrap() = Some(addr);
        Err(io::Error::other(
            "AddressRecordingConnector never dials out; this scenario only proves it was called",
        ))
    }
}

/// `Stack::new` requires an egress-check callback; no scenario in this file
/// exercises rejection by that callback (only by the private-IP guard that
/// runs before it), so this always allows.
fn always_allow_egress(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { true })
}

/// Create an `AF_UNIX SOCK_DGRAM` pair and return both ends as owned fds.
///
/// Mirrors `smoltcp_flow.rs`'s helper of the same shape.
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
/// `smoltcp_flow.rs`'s helper of the same shape.
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

/// Builds a complete Ethernet+IPv4+TCP SYN frame a guest would send to open
/// a connection to `dst_addr:dst_port`, picking that destination directly
/// rather than through any domain resolution of its own. Mirrors
/// `smoltcp_flow.rs`'s helper of the same shape.
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

/// TCP header fields parsed out of a reply frame the stack sent back to the
/// guest, the minimum this scenario needs to tell an RST apart from a
/// SYN-ACK. Mirrors `smoltcp_flow.rs`'s helper of the same shape.
struct ParsedTcpSegment {
    control: TcpControl,
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
    }
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

/// Polls `stack` until a reply frame carrying the RST control flag appears
/// on `guest_fd`, or `budget` (real wall-clock time) elapses. Mirrors
/// `smoltcp_flow.rs`'s helper of the same shape.
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

/// Polls `stack` on a short cadence until `called_with` records an address
/// or `budget` elapses, to tolerate the connector being invoked from a task
/// spawned on a later poll tick rather than synchronously. Mirrors
/// `smoltcp_flow.rs`'s helper of the same shape.
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
async fn given_flow_to_metadata_address_when_polled_then_rejected_regardless_of_resolved_label() {
    // Arrange: a Stack backed by a fake Resolver that answers an
    // allowed-looking domain with the cloud metadata address, and a fake
    // Connector that records whether it was ever called.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(MetadataAnsweringResolver);
    let called = Arc::new(Mutex::new(false));
    let connector: Box<dyn Connector> = Box::new(RecordingConnector {
        called: Arc::clone(&called),
    });
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // The guest first resolves an allowed-looking domain that an attacker
    // controlling DNS has pointed at the metadata address, so `resolved`
    // ends up recording `169.254.169.254 -> "evil.allowed.example"`.
    write_frame(&guest_fd, &build_dns_query_frame(EVIL_DOMAIN));
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(METADATA_ADDR)),
        Some(EVIL_DOMAIN),
        "resolved map should attribute the metadata address to the allowed-looking domain"
    );

    // Act: the guest picks the destination IP directly for its SYN, the
    // same metadata address the DNS answer above just resolved to.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(METADATA_ADDR, METADATA_PORT),
    );

    // Assert: the guest receives an RST rather than a SYN-ACK or silence,
    // and the connector was never dispatched for this destination.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "guest should receive an RST for a SYN to the metadata address, \
         regardless of the domain label resolved records for it"
    );
    assert!(
        !*called.lock().unwrap(),
        "connector should never be dispatched for a flow to the metadata address"
    );
}

#[tokio::test]
async fn given_flow_to_loopback_or_rfc1918_address_when_polled_then_rejected() {
    // Table-driven: loopback and RFC1918 destinations exercise the same
    // arrange/act/assert shape, so check each in one function rather than
    // duplicating the scenario per address.
    let private_destinations = [
        Ipv4Addr::new(127, 0, 0, 1),   // loopback
        Ipv4Addr::new(192, 168, 1, 1), // RFC1918
        Ipv4Addr::new(10, 0, 0, 5),    // RFC1918
    ];

    for dst in private_destinations {
        // Arrange: a Stack backed by a resolver that is never consulted
        // (the guest picks this destination directly, with no DNS lookup
        // involved) and a fake Connector that records whether it was ever
        // called.
        let (guest_fd, host_fd) = socketpair_dgram();
        let resolver: Box<dyn Resolver> = Box::new(NullResolver);
        let called = Arc::new(Mutex::new(false));
        let connector: Box<dyn Connector> = Box::new(RecordingConnector {
            called: Arc::clone(&called),
        });
        let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
        perform_arp_handshake(&mut stack, &guest_fd).await;

        // Act: the guest picks the loopback or RFC1918 destination
        // directly for its SYN.
        write_frame(&guest_fd, &build_tcp_syn_frame_to(dst, PRIVATE_DEST_PORT));

        // Assert: the guest receives an RST rather than a SYN-ACK or
        // silence, and the connector was never dispatched for this
        // destination.
        let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
        assert!(
            rst.is_some(),
            "guest should receive an RST for a SYN to {dst}, a loopback or RFC1918 address"
        );
        assert!(
            !*called.lock().unwrap(),
            "connector should never be dispatched for a flow to {dst}"
        );
    }
}

#[tokio::test]
async fn given_flow_to_public_address_when_polled_then_reaches_egress_check() {
    // Arrange: a Stack backed by a resolver that is never consulted (the
    // guest picks this destination directly, with no DNS lookup involved)
    // and a fake Connector that records the address it is asked to dial,
    // the negative case proving the guard does not over-block a
    // genuinely public destination.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let called_with = Arc::new(Mutex::new(None));
    let connector: Box<dyn Connector> = Box::new(AddressRecordingConnector {
        called_with: Arc::clone(&called_with),
    });
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: the guest picks a genuinely public destination directly for its
    // SYN.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(PUBLIC_DEST_ADDR, PUBLIC_DEST_PORT),
    );

    // Assert: the connector is dispatched with the exact destination the
    // guest's SYN carried, proving the flow reached the egress check
    // instead of being rejected by the private/loopback/link-local guard.
    let addr = poll_until_connector_called(&mut stack, &called_with, Duration::from_secs(2))
        .await
        .expect(
            "stack should call the connector for a SYN to a public address within the poll budget",
        );
    assert_eq!(
        addr,
        SocketAddr::new(IpAddr::V4(PUBLIC_DEST_ADDR), PUBLIC_DEST_PORT),
        "connector should be called with the exact public destination address the guest's SYN carried"
    );
}
