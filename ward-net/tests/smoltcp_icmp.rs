// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ICMP echo tests for `Stack`.
//!
//! Drives a guest-side ICMP echo request (ping) through the raw socketpair
//! harness and asserts `Stack` answers with an echo reply carrying the same
//! identifier and sequence number the request carried.

#![cfg(feature = "smoltcp")]

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpProtocol, Ipv4Packet, Ipv4Repr,
};
use ward_net::smoltcp_backend::{Resolver, Stack};

/// MAC `Stack`'s interface already answers on (mirrors the private
/// `INTERFACE_HARDWARE_ADDR` constant in `smoltcp_backend`), so a guest
/// frame addressed here is accepted instead of dropped as a MAC mismatch.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface must own for a guest echo request
/// addressed here to be accepted rather than dropped.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Pre-DHCP guest address for this test; DHCP itself is a separate
/// scenario, so this is only ever used as the request's source address.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// Identifier this scenario's echo request carries, distinct from
/// `ECHO_SEQ_NO` so a test that accidentally swapped the two fields would
/// fail rather than pass by coincidence.
const ECHO_IDENT: u16 = 0x4242;
/// Sequence number this scenario's echo request carries.
const ECHO_SEQ_NO: u16 = 7;

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

/// Polls `stack` and checks `guest_fd` for a reply, retrying on a short
/// cadence until `budget` elapses, to tolerate a reply that is only sent on
/// a later poll tick.
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

/// Builds a complete Ethernet+IPv4+ICMP echo request frame a guest would
/// send to ping `GATEWAY_ADDR`, with a correctly computed IPv4/ICMP
/// checksum so the checksum validation smoltcp runs by default on ingress
/// accepts it.
fn build_icmp_echo_request_frame() -> Vec<u8> {
    let icmp_repr = Icmpv4Repr::EchoRequest {
        ident: ECHO_IDENT,
        seq_no: ECHO_SEQ_NO,
        data: b"ward-icmp-test-payload",
    };
    let icmp_len = icmp_repr.buffer_len();

    let ip_repr = Ipv4Repr {
        src_addr: GUEST_ADDR,
        dst_addr: GATEWAY_ADDR,
        next_header: IpProtocol::Icmp,
        payload_len: icmp_len,
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: GATEWAY_MAC,
        ethertype: EthernetProtocol::Ipv4,
    };

    let total_len = eth_repr.buffer_len() + ip_repr.buffer_len() + icmp_len;
    let mut buf = vec![0u8; total_len];

    let mut eth_frame = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth_frame);

    let mut ip_packet = Ipv4Packet::new_unchecked(eth_frame.payload_mut());
    ip_repr.emit(&mut ip_packet, &ChecksumCapabilities::default());

    let mut icmp_packet = Icmpv4Packet::new_unchecked(ip_packet.payload_mut());
    icmp_repr.emit(&mut icmp_packet, &ChecksumCapabilities::default());

    buf
}

/// Parses a reply frame, asserting it addresses the guest correctly, and
/// returns the echo reply's identifier and sequence number.
fn parse_icmp_echo_reply(frame: &[u8]) -> (u16, u16) {
    let eth_frame =
        EthernetFrame::new_checked(frame).expect("reply should be a valid Ethernet frame");
    assert_eq!(
        eth_frame.dst_addr(),
        GUEST_MAC,
        "reply should be addressed back to the guest's MAC"
    );
    assert_eq!(eth_frame.ethertype(), EthernetProtocol::Ipv4);

    let ip_packet =
        Ipv4Packet::new_checked(eth_frame.payload()).expect("reply should be a valid IPv4 packet");
    assert_eq!(ip_packet.next_header(), IpProtocol::Icmp);
    assert_eq!(
        ip_packet.src_addr(),
        GATEWAY_ADDR,
        "reply should originate from the stack's own address"
    );
    assert_eq!(
        ip_packet.dst_addr(),
        GUEST_ADDR,
        "reply should be addressed back to the guest's own address"
    );

    let icmp_packet = Icmpv4Packet::new_checked(ip_packet.payload())
        .expect("reply should be a valid ICMP packet");
    match Icmpv4Repr::parse(&icmp_packet, &ChecksumCapabilities::default())
        .expect("reply should parse as an ICMP message")
    {
        Icmpv4Repr::EchoReply { ident, seq_no, .. } => (ident, seq_no),
        other => panic!("expected an ICMP echo reply, got {other:?}"),
    }
}

#[tokio::test]
async fn given_guest_icmp_echo_request_when_polled_then_echo_reply_returned_with_matching_identifier_and_sequence()
 {
    // Arrange: a Stack backed by the socketpair harness; ICMP never
    // consults the resolver, so a resolver that always answers empty is
    // sufficient to satisfy Stack::new's constructor injection.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let mut stack = Stack::new(host_fd, resolver);
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let echo_request = build_icmp_echo_request_frame();
    write_frame(&guest_fd, &echo_request);

    // Act
    let reply = poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should reply to the guest's ICMP echo request within the poll budget");
    let (ident, seq_no) = parse_icmp_echo_reply(&reply);

    // Assert
    assert_eq!(
        ident, ECHO_IDENT,
        "echo reply should carry the request's own identifier"
    );
    assert_eq!(
        seq_no, ECHO_SEQ_NO,
        "echo reply should carry the request's own sequence number"
    );
}
