// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Egress-allowlist wiring tests for `Stack`.
//!
//! Drives a guest-side DNS query and a guest-side TCP SYN through the raw
//! socketpair harness and asserts `Stack` consults an injected egress-check
//! callback with the flow's resolved domain label and port before dialing
//! out via `Connector`.

#![cfg(feature = "smoltcp")]

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
/// Domain this scenario's allowlist permits; the fake egress-check callback
/// answers `true` for it, standing in for an allowlist entry that matches.
const ALLOWED_DOMAIN: &str = "allowed.example.com";
/// A real but unrelated public IP used only as an address label, never
/// actually dialed since the fake `Connector` intercepts every connect
/// attempt. Mirrors `smoltcp_flow.rs`'s `DEST_ADDR`.
const ALLOWED_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
const ALLOWED_PORT: u16 = 443;
/// Domain this scenario's allowlist denies; the fake egress-check callback
/// answers `false` for it, standing in for a domain no allowlist entry
/// matches.
const DENIED_DOMAIN: &str = "denied.example.com";
/// A real but unrelated public IP used only as an address label; never
/// actually dialed, since this scenario asserts the fake `Connector` is
/// never even called.
const DENIED_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 35);
const DENIED_PORT: u16 = 443;
/// A public IP the guest connects to directly, with no preceding DNS query
/// through this scenario's `Stack`, so `resolved` never learns a domain
/// label for it.
const UNRESOLVED_ADDR: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 36);
const UNRESOLVED_PORT: u16 = 443;

/// Future type an egress-check callback returns; boxed and pinned since the
/// callback is stored behind a trait object and must be `Send` to cross
/// into the poll loop's spawned tasks.
type EgressCheckFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Callback type `Stack::new`'s fourth argument expects, aliased so the
/// call site below doesn't repeat this trait object inline.
type EgressCheckFn = Box<dyn Fn(&str, u16) -> EgressCheckFuture + Send + Sync>;

/// Fake `Resolver` answering `ALLOWED_DOMAIN` with a public address,
/// standing in for a real DNS lookup so the guest's SYN below has a domain
/// label recorded in `resolved` for the egress check to consult.
struct AllowedDomainResolver;

#[async_trait::async_trait]
impl Resolver for AllowedDomainResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        if name == ALLOWED_DOMAIN {
            vec![IpAddr::V4(ALLOWED_ADDR)]
        } else {
            Vec::new()
        }
    }
}

/// Fake `Connector` that counts how many times it was invoked, so a test
/// can assert an allowed flow reaches the connect stage exactly once.
struct CountingConnector {
    call_count: Arc<Mutex<u32>>,
}

#[async_trait::async_trait]
impl Connector for CountingConnector {
    async fn connect(&self, _addr: SocketAddr) -> io::Result<TcpStream> {
        *self.call_count.lock().unwrap() += 1;
        Err(io::Error::other(
            "CountingConnector never dials out; this scenario only proves it was called",
        ))
    }
}

/// Fake `Resolver` answering `DENIED_DOMAIN` with a public address,
/// standing in for a real DNS lookup so the guest's SYN below has a domain
/// label recorded in `resolved` for the egress check to consult.
struct DeniedDomainResolver;

#[async_trait::async_trait]
impl Resolver for DeniedDomainResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        if name == DENIED_DOMAIN {
            vec![IpAddr::V4(DENIED_ADDR)]
        } else {
            Vec::new()
        }
    }
}

/// Fake `Resolver` that never resolves anything, standing in for a guest
/// that connects to a literal IP without this `Stack` ever having relayed a
/// DNS query for it.
struct NeverResolvingResolver;

#[async_trait::async_trait]
impl Resolver for NeverResolvingResolver {
    async fn resolve(&self, _name: &str) -> Vec<IpAddr> {
        Vec::new()
    }
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
/// Mirrors `smoltcp_ssrf_guard.rs`'s helper of the same shape.
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
/// `smoltcp_ssrf_guard.rs`'s helper of the same shape.
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

/// Polls `stack` and checks `guest_fd` for a relayed response, retrying on
/// a short cadence until `budget` elapses, to tolerate a relay that
/// resolves the query asynchronously across more than one poll tick.
/// Mirrors `smoltcp_ssrf_guard.rs`'s helper of the same shape.
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

/// TCP header fields parsed out of a reply frame the stack sent back to the
/// guest, the minimum this scenario needs to tell an RST apart from a
/// SYN-ACK. Mirrors `smoltcp_ssrf_guard.rs`'s helper of the same shape.
struct ParsedTcpSegment {
    control: TcpControl,
}

/// Parses `frame` as an Ethernet+IPv4+TCP frame, panicking if it isn't one:
/// a malformed frame here is a test bug, not something worth handling
/// gracefully. Mirrors `smoltcp_ssrf_guard.rs`'s helper of the same shape.
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

/// Polls `stack` until a reply frame carrying the RST control flag appears
/// on `guest_fd`, or `budget` (real wall-clock time) elapses. Mirrors
/// `smoltcp_ssrf_guard.rs`'s helper of the same shape.
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

/// Polls `stack` on a short cadence until `call_count` reaches at least one
/// call or `budget` elapses, to tolerate the connector being invoked from a
/// task spawned on a later poll tick rather than synchronously.
async fn poll_until_connector_called(
    stack: &mut Stack,
    call_count: &Arc<Mutex<u32>>,
    budget: Duration,
) -> u32 {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        stack.poll();
        let count = *call_count.lock().unwrap();
        if count > 0 {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    *call_count.lock().unwrap()
}

#[tokio::test]
async fn given_allowlist_policy_when_flow_to_allowed_domain_then_fake_connector_called_once() {
    // Arrange: a Stack backed by a fake Resolver that resolves
    // ALLOWED_DOMAIN to a public address, a fake egress-check callback
    // standing in for an allowlist that permits ALLOWED_DOMAIN on any port
    // it is asked about, and a fake Connector that counts how many times
    // it was dialed.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(AllowedDomainResolver);
    let call_count = Arc::new(Mutex::new(0u32));
    let connector: Box<dyn Connector> = Box::new(CountingConnector {
        call_count: Arc::clone(&call_count),
    });
    let egress_calls: Arc<Mutex<Vec<(String, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let egress_calls_for_closure = Arc::clone(&egress_calls);
    let egress_check: EgressCheckFn = Box::new(move |domain: &str, port: u16| {
        egress_calls_for_closure
            .lock()
            .unwrap()
            .push((domain.to_string(), port));
        Box::pin(async { true })
    });
    let mut stack = Stack::new(host_fd, resolver, connector, egress_check);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // The guest first resolves the allowed domain, so `resolved` ends up
    // recording `ALLOWED_ADDR -> ALLOWED_DOMAIN`.
    write_frame(&guest_fd, &build_dns_query_frame(ALLOWED_DOMAIN));
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(ALLOWED_ADDR)),
        Some(ALLOWED_DOMAIN),
        "resolved map should attribute the allowed address to the allowed domain"
    );

    // Act: the guest opens a SYN to the resolved address.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(ALLOWED_ADDR, ALLOWED_PORT),
    );

    // Assert: the connector was dispatched exactly once, and the
    // egress-check callback was consulted with the resolved domain label
    // and the flow's destination port.
    let count = poll_until_connector_called(&mut stack, &call_count, Duration::from_secs(2)).await;
    assert_eq!(
        count, 1,
        "fake connector should be called exactly once for a flow to an allowed domain"
    );
    assert_eq!(
        *egress_calls.lock().unwrap(),
        vec![(ALLOWED_DOMAIN.to_string(), ALLOWED_PORT)],
        "egress-check callback should be called once with the resolved domain and destination port"
    );
}

#[tokio::test]
async fn given_allowlist_policy_when_flow_to_denied_domain_then_rst_and_fake_connector_never_called()
 {
    // Arrange: same shape as the allowed-domain scenario, but the fake
    // egress-check callback answers `false` for every domain it is asked
    // about, standing in for an allowlist that denies DENIED_DOMAIN.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(DeniedDomainResolver);
    let call_count = Arc::new(Mutex::new(0u32));
    let connector: Box<dyn Connector> = Box::new(CountingConnector {
        call_count: Arc::clone(&call_count),
    });
    let egress_check: EgressCheckFn =
        Box::new(|_domain: &str, _port: u16| Box::pin(async { false }));
    let mut stack = Stack::new(host_fd, resolver, connector, egress_check);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // The guest first resolves the denied domain, so `resolved` ends up
    // recording `DENIED_ADDR -> DENIED_DOMAIN`.
    write_frame(&guest_fd, &build_dns_query_frame(DENIED_DOMAIN));
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(DENIED_ADDR)),
        Some(DENIED_DOMAIN),
        "resolved map should attribute the denied address to the denied domain"
    );

    // Act: the guest opens a SYN to the resolved (denied) address.
    write_frame(&guest_fd, &build_tcp_syn_frame_to(DENIED_ADDR, DENIED_PORT));

    // Assert: the guest receives an RST rather than a SYN-ACK, and the fake
    // connector is never called since the egress check rejected the flow
    // before any connect attempt was made.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "guest should receive an RST for a SYN to a denied domain"
    );
    assert_eq!(
        *call_count.lock().unwrap(),
        0,
        "fake connector should never be called for a flow to a denied domain"
    );
}

#[tokio::test]
async fn given_unresolved_destination_ip_when_checked_then_denied() {
    // Arrange: a Stack backed by a fake Resolver that never resolves
    // anything (this scenario never drives a DNS query), and a fake
    // egress-check callback that records every domain string it is asked
    // about and denies anything IP-shaped, standing in for a real
    // EgressProxy's pattern-based allowlist naturally failing to match a
    // bare IP address against its domain patterns.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NeverResolvingResolver);
    let call_count = Arc::new(Mutex::new(0u32));
    let connector: Box<dyn Connector> = Box::new(CountingConnector {
        call_count: Arc::clone(&call_count),
    });
    let egress_calls: Arc<Mutex<Vec<(String, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let egress_calls_for_closure = Arc::clone(&egress_calls);
    let egress_check: EgressCheckFn = Box::new(move |domain: &str, port: u16| {
        egress_calls_for_closure
            .lock()
            .unwrap()
            .push((domain.to_string(), port));
        let is_bare_ip = domain.parse::<IpAddr>().is_ok();
        Box::pin(async move { !is_bare_ip })
    });
    let mut stack = Stack::new(host_fd, resolver, connector, egress_check);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: the guest opens a SYN straight to a public IP it never looked up
    // through this Stack's DNS relay.
    write_frame(
        &guest_fd,
        &build_tcp_syn_frame_to(UNRESOLVED_ADDR, UNRESOLVED_PORT),
    );

    // Assert: the guest receives an RST, the egress-check callback was
    // consulted with the bare IP as the domain string (the fallback the
    // Goal describes for an unresolved destination), and the fake
    // connector is never called.
    let rst = poll_until_rst(&mut stack, &guest_fd, Duration::from_secs(2)).await;
    assert!(
        rst.is_some(),
        "guest should receive an RST for a SYN to an address that was never resolved"
    );
    assert_eq!(
        *egress_calls.lock().unwrap(),
        vec![(UNRESOLVED_ADDR.to_string(), UNRESOLVED_PORT)],
        "egress-check callback should be called with the bare IP as the domain string \
         when the destination was never resolved via DNS"
    );
    assert_eq!(
        *call_count.lock().unwrap(),
        0,
        "fake connector should never be called for a flow to an unresolved destination"
    );
}
