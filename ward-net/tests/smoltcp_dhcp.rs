// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! DHCP server tests for `Stack`.
//!
//! Drives a guest-side DHCP DISCOVER through the raw socketpair harness and
//! asserts `Stack` answers with an OFFER carrying a leasable address and its
//! own address as the default gateway. smoltcp's `socket::dhcpv4::Socket` is
//! a DHCP client only, so `Stack` must serve DHCP by hand-building the wire
//! packets via `smoltcp::wire::dhcpv4` (which does cover both directions).

#![cfg(feature = "smoltcp")]

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress,
    EthernetFrame, EthernetProtocol, EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr,
    UdpPacket, UdpRepr,
};
use ward_net::smoltcp_backend::{Resolver, Stack};

/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC. A DHCP
/// DISCOVER needs no destination MAC of its own kind (it broadcasts), but
/// the client's own hardware address is carried in the DHCP payload.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface owns; the scenario asserts the OFFER's
/// router option equals this, i.e. `Stack` hands out itself as the gateway.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DISCOVER_TRANSACTION_ID: u32 = 0xc0ff_ee42;

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

/// Builds a complete Ethernet+IPv4+UDP+DHCP DISCOVER frame, broadcast at
/// both the Ethernet and IP layers (as a client with no address of its own
/// must), with correctly computed IPv4/UDP checksums so the checksum
/// validation smoltcp runs by default on ingress accepts it.
fn build_dhcp_discover_frame() -> Vec<u8> {
    let dhcp_repr = DhcpRepr {
        message_type: DhcpMessageType::Discover,
        transaction_id: DISCOVER_TRANSACTION_ID,
        secs: 0,
        client_hardware_address: GUEST_MAC,
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip: Ipv4Addr::UNSPECIFIED,
        server_ip: Ipv4Addr::UNSPECIFIED,
        router: None,
        subnet_mask: None,
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        broadcast: true,
        requested_ip: None,
        client_identifier: Some(GUEST_MAC),
        server_identifier: None,
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        lease_duration: None,
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };
    let dhcp_len = dhcp_repr.buffer_len();

    let udp_repr = UdpRepr {
        src_port: DHCP_CLIENT_PORT,
        dst_port: DHCP_SERVER_PORT,
    };
    let ip_payload_len = udp_repr.header_len() + dhcp_len;

    let ip_repr = Ipv4Repr {
        src_addr: Ipv4Addr::UNSPECIFIED,
        dst_addr: Ipv4Addr::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: ip_payload_len,
        hop_limit: 64,
    };

    let eth_repr = EthernetRepr {
        src_addr: GUEST_MAC,
        dst_addr: EthernetAddress::BROADCAST,
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
        &IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED),
        &IpAddress::Ipv4(Ipv4Addr::BROADCAST),
        dhcp_len,
        |dhcp_buf| {
            let mut dhcp_packet = DhcpPacket::new_unchecked(dhcp_buf);
            dhcp_repr
                .emit(&mut dhcp_packet)
                .expect("dhcp repr should fit exactly in its own buffer_len");
        },
        &ChecksumCapabilities::default(),
    );

    buf
}

/// Parses a DHCP reply frame down to the fields this scenario checks, so the
/// test does not also have to assert an implementation choice (broadcast
/// vs.\ unicast reply) that is left open here.
fn parse_dhcp_reply(frame: &[u8]) -> (u32, DhcpMessageType, Ipv4Addr, Option<Ipv4Addr>) {
    let eth_frame =
        EthernetFrame::new_checked(frame).expect("reply should be a valid Ethernet frame");
    assert_eq!(eth_frame.ethertype(), EthernetProtocol::Ipv4);

    let ip_packet =
        Ipv4Packet::new_checked(eth_frame.payload()).expect("reply should be a valid IPv4 packet");
    assert_eq!(ip_packet.next_header(), IpProtocol::Udp);

    let udp_packet =
        UdpPacket::new_checked(ip_packet.payload()).expect("reply should be a valid UDP datagram");
    assert_eq!(
        udp_packet.src_port(),
        DHCP_SERVER_PORT,
        "reply should originate from the DHCP server port"
    );
    assert_eq!(
        udp_packet.dst_port(),
        DHCP_CLIENT_PORT,
        "reply should be addressed back to the DHCP client port"
    );

    let dhcp_packet =
        DhcpPacket::new_checked(udp_packet.payload()).expect("reply should be a valid DHCP packet");
    let repr = DhcpRepr::parse(&dhcp_packet).expect("reply should parse as a DHCP message");
    (
        repr.transaction_id,
        repr.message_type,
        repr.your_ip,
        repr.router,
    )
}

#[tokio::test]
async fn given_dhcp_discover_when_polled_then_guest_gets_offer_with_gateway() {
    // Arrange: a Stack backed by the socketpair harness; DHCP never
    // consults the resolver, so a resolver that always answers empty is
    // sufficient to satisfy Stack::new's constructor injection.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(NullResolver);
    let mut stack = Stack::new(host_fd, resolver);

    let discover = build_dhcp_discover_frame();
    write_frame(&guest_fd, &discover);

    // Act
    let reply = poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should reply to the guest's DHCP DISCOVER within the poll budget");
    let (transaction_id, message_type, offered_ip, router) = parse_dhcp_reply(&reply);

    // Assert
    assert_eq!(
        transaction_id, DISCOVER_TRANSACTION_ID,
        "reply should carry the DISCOVER's own transaction ID"
    );
    assert_eq!(
        message_type,
        DhcpMessageType::Offer,
        "stack should answer a DISCOVER with an OFFER"
    );
    assert_ne!(
        offered_ip,
        Ipv4Addr::UNSPECIFIED,
        "offer should include a leasable address for the guest"
    );
    assert_eq!(
        router,
        Some(GATEWAY_ADDR),
        "offer should hand back the stack's own address as the default gateway"
    );
}
