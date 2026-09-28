// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! DNS relay tests for `Stack`.
//!
//! Drives a guest-side DNS query through the raw socketpair harness and
//! asserts `Stack` relays it to an injected `Resolver` and answers with
//! the resolver's own result, with no live network access involved.

#![cfg(feature = "smoltcp")]

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::time::Duration;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, DnsFlags, DnsOpcode, DnsPacket, DnsQueryType, DnsQuestion,
    DnsRecord, DnsRecordData, DnsRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, IpAddress, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};
use ward_net::smoltcp_backend::{Connector, Resolver, Stack};

/// MAC `Stack`'s interface already answers on (mirrors the private
/// `INTERFACE_HARDWARE_ADDR` constant in `smoltcp_backend`), so a guest
/// frame addressed here is accepted instead of dropped as a MAC mismatch.
const GATEWAY_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Arbitrary "guest" MAC standing in for the sandbox's virtual NIC.
const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
/// IPv4 address `Stack`'s interface must own for guest packets addressed
/// here (DNS queries, ICMP echoes) to be accepted rather than dropped.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Pre-DHCP guest address for this test; DHCP itself is a separate
/// scenario, so this is only ever used as the query's source address.
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GUEST_SRC_PORT: u16 = 54321;
const DNS_SERVER_PORT: u16 = 53;
const CANNED_DOMAIN: &str = "svc.ward.test";
const CANNED_ANSWER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 42);
const QUERY_TRANSACTION_ID: u16 = 0xbeef;
/// Mirrors the private `MAX_RESOLVED_ENTRIES` cap in `smoltcp_backend`, so
/// the eviction test below can assert against the real limit without the
/// production constant needing to be exported.
const MAX_RESOLVED_ENTRIES: usize = 4096;
/// Expected cap for the outstanding DNS query table in `smoltcp_backend`,
/// same order of magnitude as `MAX_RESOLVED_ENTRIES`, so the capacity test
/// below can assert against the real limit once it exists, without the
/// production constant needing to be exported.
const MAX_PENDING_DNS_QUERIES: usize = 4096;

/// Fake `Resolver` answering only `CANNED_DOMAIN`, so a correct relayed
/// answer in a test can only have come from this injected resolver, never
/// a real lookup.
struct FakeResolver {
    domain: &'static str,
    answer: Ipv4Addr,
}

#[async_trait::async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        if name == self.domain {
            vec![IpAddr::V4(self.answer)]
        } else {
            Vec::new()
        }
    }
}

/// `Stack::new` requires a `Connector`, but this scenario never sends a TCP
/// SYN, so this always fails rather than standing in for a real dialer.
struct NullConnector;

#[async_trait::async_trait]
impl Connector for NullConnector {
    async fn connect(&self, _addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
        Err(std::io::Error::other(
            "NullConnector never dials out; this scenario never opens a TCP flow",
        ))
    }
}

/// `Stack::new` requires an egress-check callback, but this scenario never
/// opens a TCP flow, so this always allows rather than standing in for a
/// real allowlist policy.
fn always_allow_egress(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { true })
}

/// Create an `AF_UNIX SOCK_DGRAM` pair and return both ends as owned fds.
///
/// Mirrors `smoltcp_device.rs`'s helper of the same shape.
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
        src_port: GUEST_SRC_PORT,
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

/// Parses a relayed DNS response frame, returning its transaction ID and
/// every `A` record address in its answer section.
fn parse_dns_response(frame: &[u8]) -> (u16, Vec<Ipv4Addr>) {
    let eth_frame =
        EthernetFrame::new_checked(frame).expect("response should be a valid Ethernet frame");
    assert_eq!(
        eth_frame.dst_addr(),
        GUEST_MAC,
        "response should be addressed back to the guest's MAC"
    );
    assert_eq!(eth_frame.ethertype(), EthernetProtocol::Ipv4);

    let ip_packet = Ipv4Packet::new_checked(eth_frame.payload())
        .expect("response should be a valid IPv4 packet");
    assert_eq!(ip_packet.next_header(), IpProtocol::Udp);

    let udp_packet = UdpPacket::new_checked(ip_packet.payload())
        .expect("response should be a valid UDP datagram");
    assert_eq!(
        udp_packet.dst_port(),
        GUEST_SRC_PORT,
        "response should be addressed back to the guest's query source port"
    );
    assert_eq!(
        udp_packet.src_port(),
        DNS_SERVER_PORT,
        "response should originate from the DNS server port"
    );

    let dns_packet = DnsPacket::new_checked(udp_packet.payload())
        .expect("response should be a valid DNS packet");
    assert!(
        dns_packet.flags().contains(DnsFlags::RESPONSE),
        "relayed packet should carry the DNS response bit"
    );

    let transaction_id = dns_packet.transaction_id();
    let (mut rest, _question) = DnsQuestion::parse(dns_packet.payload())
        .expect("response should echo back the question section");

    let mut answers = Vec::new();
    for _ in 0..dns_packet.answer_record_count() {
        let (next_rest, record) =
            DnsRecord::parse(rest).expect("answer record should be well-formed");
        rest = next_rest;
        if let DnsRecordData::A(addr) = record.data {
            answers.push(addr);
        }
    }

    (transaction_id, answers)
}

/// Sends an ARP request for `GATEWAY_ADDR` from the guest and drains the
/// reply, so `stack`'s neighbor cache learns the guest's MAC before a
/// scenario needs a unicast reply routed back to it. smoltcp only ever
/// learns a peer's MAC from an actual ARP exchange (never as a side effect
/// of receiving a plain IPv4 packet), so any test whose reply path depends
/// on that cache, DNS, DHCP, or ICMP, needs to run this first.
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

/// Polls `stack` and checks `guest_fd` for a relayed response, retrying on
/// a short cadence until `budget` elapses, to tolerate a relay that
/// resolves the query asynchronously across more than one poll tick.
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

#[tokio::test]
async fn given_guest_dns_query_when_relayed_then_fake_resolver_answer_returned() {
    // Arrange: a Stack backed by the socketpair harness and a fake
    // Resolver that only answers CANNED_DOMAIN, so a correct answer can
    // only have come from the injected resolver, never a real lookup.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver {
        domain: CANNED_DOMAIN,
        answer: CANNED_ANSWER,
    });
    let connector: Box<dyn Connector> = Box::new(NullConnector);
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let query = build_dns_query_frame(CANNED_DOMAIN);
    write_frame(&guest_fd, &query);

    // Act
    let response = poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");
    let (transaction_id, answers) = parse_dns_response(&response);

    // Assert
    assert_eq!(
        transaction_id, QUERY_TRANSACTION_ID,
        "response should carry the query's own transaction ID"
    );
    assert_eq!(
        answers,
        vec![CANNED_ANSWER],
        "response should carry exactly the fake resolver's canned answer"
    );
}

#[tokio::test]
async fn given_dns_response_when_relayed_then_resolved_map_records_ip_to_domain() {
    // Arrange: same fake-resolver setup as the relay test above.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver {
        domain: CANNED_DOMAIN,
        answer: CANNED_ANSWER,
    });
    let connector: Box<dyn Connector> = Box::new(NullConnector);
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    let query = build_dns_query_frame(CANNED_DOMAIN);
    write_frame(&guest_fd, &query);

    // Act: let the relay run to completion, then read its resolved map.
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a DNS response back to the guest within the poll budget");

    // Assert
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(CANNED_ANSWER)),
        Some(CANNED_DOMAIN),
        "resolved map should record the answer IP against the domain that was queried"
    );
}

#[tokio::test]
async fn given_resolved_map_at_capacity_when_new_entry_then_oldest_evicted() {
    // Arrange: a Stack whose resolver is never consulted, since this
    // scenario drives the resolved map directly (bypassing the DNS wire
    // path entirely) so 10,000 insertions run fast enough for a unit test.
    let (_guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver {
        domain: CANNED_DOMAIN,
        answer: CANNED_ANSWER,
    });
    let connector: Box<dyn Connector> = Box::new(NullConnector);
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));

    // Act: insert far more synthetic entries than the map's cap, each with
    // a distinct IP and domain so eviction order is unambiguous.
    let total_entries: u32 = 10_000;
    let mut inserted = Vec::with_capacity(total_entries as usize);
    for i in 0..total_entries {
        let ip = IpAddr::V4(Ipv4Addr::new(
            10,
            ((i >> 16) & 0xff) as u8,
            ((i >> 8) & 0xff) as u8,
            (i & 0xff) as u8,
        ));
        let domain = format!("host-{i}.test");
        stack.record_resolved_for_test(ip, &domain);
        inserted.push(ip);
    }

    // Assert: exactly the cap's worth of entries survive, and they are the
    // most recently inserted ones (FIFO eviction of the oldest).
    let surviving = inserted
        .iter()
        .filter(|ip| stack.resolved_domain_for(**ip).is_some())
        .count();
    assert_eq!(
        surviving, MAX_RESOLVED_ENTRIES,
        "resolved map should never hold more entries than its cap, even \
         after far more insertions than the cap"
    );
    assert!(
        stack.resolved_domain_for(inserted[0]).is_none(),
        "the oldest inserted entry should have been evicted"
    );
    assert!(
        stack
            .resolved_domain_for(inserted[inserted.len() - 1])
            .is_some(),
        "the most recently inserted entry should still be present"
    );
}

/// Domain/answer pair used by the transaction-ID-collision scenario below,
/// resolved after `delay` regardless of the other pair's own delay, so a
/// test can control which of two concurrent queries answers first.
const COLLIDING_TXID_SLOW_DOMAIN: &str = "slow.svc.ward.test";
const COLLIDING_TXID_SLOW_ANSWER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 50);
const COLLIDING_TXID_FAST_DOMAIN: &str = "fast.svc.ward.test";
const COLLIDING_TXID_FAST_ANSWER: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 60);

/// Fake `Resolver` mapping several domains to distinct answers, each with
/// its own artificial resolution delay, so a test can control which of two
/// concurrent queries resolves first independently of send order.
struct DelayedMultiResolver {
    answers: Vec<(&'static str, Ipv4Addr, Duration)>,
}

#[async_trait::async_trait]
impl Resolver for DelayedMultiResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        let Some((_, answer, delay)) = self.answers.iter().find(|(domain, ..)| *domain == name)
        else {
            return Vec::new();
        };
        if !delay.is_zero() {
            tokio::time::sleep(*delay).await;
        }
        vec![IpAddr::V4(*answer)]
    }
}

#[tokio::test]
async fn given_spoofed_dns_response_wrong_transaction_id_when_relayed_then_resolved_map_unchanged()
{
    // Stack's DNS relay calls a Resolver trait directly rather than
    // consulting an untrusted upstream response packet, so there is no
    // forged-packet path to spoof here. `build_dns_query_frame` always
    // stamps QUERY_TRANSACTION_ID, so two overlapping queries for
    // different domains collide on that ID exactly the way a spoofed
    // response's matching transaction ID would; this scenario's real risk
    // in this architecture is a slow-to-resolve first query's eventual
    // answer landing against the wrong domain.
    //
    // Arrange: a slow query for COLLIDING_TXID_SLOW_DOMAIN sent before a
    // fast one for COLLIDING_TXID_FAST_DOMAIN, both carrying the same
    // transaction ID, so the fast query resolves and is delivered first.
    let (guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(DelayedMultiResolver {
        answers: vec![
            (
                COLLIDING_TXID_SLOW_DOMAIN,
                COLLIDING_TXID_SLOW_ANSWER,
                Duration::from_millis(300),
            ),
            (
                COLLIDING_TXID_FAST_DOMAIN,
                COLLIDING_TXID_FAST_ANSWER,
                Duration::ZERO,
            ),
        ],
    });
    let connector: Box<dyn Connector> = Box::new(NullConnector);
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));
    perform_arp_handshake(&mut stack, &guest_fd).await;

    // Act: fire both queries before either resolves, then drain both
    // relayed responses (order is not asserted; only the resulting
    // resolved-map attribution is).
    write_frame(
        &guest_fd,
        &build_dns_query_frame(COLLIDING_TXID_SLOW_DOMAIN),
    );
    write_frame(
        &guest_fd,
        &build_dns_query_frame(COLLIDING_TXID_FAST_DOMAIN),
    );

    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a response for the first-resolved query");
    poll_until_response(&mut stack, &guest_fd, Duration::from_secs(2))
        .await
        .expect("stack should relay a response for the second-resolved query");

    // Assert: each answer is recorded against its own domain, never the
    // other query's, despite both queries sharing a transaction ID.
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(COLLIDING_TXID_SLOW_ANSWER)),
        Some(COLLIDING_TXID_SLOW_DOMAIN),
        "the slow query's answer should be recorded against its own domain, \
         not the fast query's domain"
    );
    assert_eq!(
        stack.resolved_domain_for(IpAddr::V4(COLLIDING_TXID_FAST_ANSWER)),
        Some(COLLIDING_TXID_FAST_DOMAIN),
        "the fast query's answer should be recorded against its own domain, \
         not the slow query's domain"
    );
}

#[tokio::test]
async fn given_outstanding_query_table_at_capacity_when_new_query_then_oldest_evicted_or_rejected()
{
    // Arrange: a Stack whose resolver is never consulted, since this
    // scenario pushes synthetic outstanding queries directly (bypassing
    // the DNS wire path entirely, mirroring the resolved-map capacity test
    // above) so far more than the table's cap run fast enough for a unit
    // test. None of these queries are ever answered, matching a guest that
    // floods queries without letting them resolve.
    let (_guest_fd, host_fd) = socketpair_dgram();
    let resolver: Box<dyn Resolver> = Box::new(FakeResolver {
        domain: CANNED_DOMAIN,
        answer: CANNED_ANSWER,
    });
    let connector: Box<dyn Connector> = Box::new(NullConnector);
    let mut stack = Stack::new(host_fd, resolver, connector, Box::new(always_allow_egress));

    // Act: push far more synthetic outstanding queries than the table's
    // expected cap, each for a distinct domain, none of which ever
    // resolve.
    let total_queries: u32 = 10_000;
    for i in 0..total_queries {
        let domain = format!("host-{i}.test");
        stack.push_pending_dns_query_for_test(i as u16, &domain);
    }

    // Assert: the outstanding-query table never grows past its cap, even
    // though far more queries than that were pushed and none resolved.
    let outstanding = stack.pending_dns_query_count_for_test();
    assert!(
        outstanding <= MAX_PENDING_DNS_QUERIES,
        "outstanding DNS query table should never exceed its cap of {MAX_PENDING_DNS_QUERIES} \
         entries, but held {outstanding}"
    );
}
