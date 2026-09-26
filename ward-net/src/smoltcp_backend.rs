// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! smoltcp backend — research path.
//!
//! Per ADR-018, smoltcp is not on the v0.1 critical path. This module
//! exists so the [`crate::NetworkBackend`] trait shape covers all three
//! candidates uniformly and so future work has a deliberate starting
//! point (rather than discovering, six months from now, that smoltcp
//! needs a different trait surface than passt).
//!
//! [`RawFdDevice`] implements smoltcp's `phy::Device` trait over a raw
//! file descriptor (a `socketpair(2)` end), reading and writing raw
//! Ethernet frames. [`SmoltcpBackend`] (the [`NetworkBackend`] impl) does
//! not yet wire a device into a running `Interface`:
//! - Implements `probe()` (smoltcp is in-process so probing always
//!   succeeds).
//! - `attach` / `detach` return `Error::Unimplemented` with a pointer at
//!   ADR-018's "Future work" section.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpMessageType, DhcpPacket, DhcpRepr, DnsFlags, DnsPacket,
    DnsQueryType, DnsQuestion, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint,
};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{AttachId, AttachOptions, Error, NetworkBackend};

/// Resolves a domain name to zero or more addresses on the guest's behalf.
/// [`Stack`] never resolves names itself: every guest DNS query is handed
/// to an injected `Resolver` (a real one in production, a scripted one in
/// tests) so the relay logic never depends on live network access.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, name: &str) -> Vec<IpAddr>;
}

/// Production [`Resolver`] backed by the host's own resolver via
/// `getaddrinfo(3)` (through `tokio::net::lookup_host`, which runs it on a
/// blocking thread so it never stalls the stack's poll loop).
#[derive(Debug, Default)]
pub struct SystemResolver;

#[async_trait::async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, name: &str) -> Vec<IpAddr> {
        match tokio::net::lookup_host((name, 0)).await {
            Ok(addrs) => addrs.map(|socket_addr| socket_addr.ip()).collect(),
            Err(err) => {
                tracing::warn!(name, error = %err, "system DNS resolution failed");
                Vec::new()
            }
        }
    }
}

/// Largest Ethernet frame `RawFdDevice` will read or write: the standard
/// 1500-octet IP MTU plus the 14-octet Ethernet header.
const MAX_FRAME_LEN: usize = 1514;

/// Smallest valid Ethernet frame: 6-byte destination MAC, 6-byte source
/// MAC, 2-byte ethertype. A datagram shorter than this cannot be parsed
/// as a frame and is dropped.
const MIN_ETHERNET_FRAME_LEN: usize = 14;

/// A smoltcp `phy::Device` that reads and writes raw Ethernet frames on
/// an `OwnedFd` (typically one end of an `AF_UNIX SOCK_DGRAM` pair).
pub struct RawFdDevice {
    fd: OwnedFd,
}

impl RawFdDevice {
    pub fn new(fd: OwnedFd) -> RawFdDevice {
        RawFdDevice { fd }
    }
}

impl Device for RawFdDevice {
    type RxToken<'a>
        = RawFdRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = RawFdTxToken
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Loops past a malformed datagram instead of returning None for
        // it: the recv below already consumed that datagram, so reporting
        // None (meaning "queue empty") would make the poll loop stop
        // early and leave any valid frame queued behind it waiting for
        // the next tick. Only a genuinely empty queue (EAGAIN, n <= 0)
        // returns None.
        loop {
            // Sized one byte past MAX_FRAME_LEN so an oversized datagram
            // (the kernel silently truncates SOCK_DGRAM reads to the
            // buffer size) fills the whole buffer and is distinguishable
            // from a frame that legitimately fills exactly MAX_FRAME_LEN
            // bytes.
            let mut buf = [0u8; MAX_FRAME_LEN + 1];
            // SAFETY: self.fd is a valid open fd for the device's
            // lifetime; buf is a valid, initialized buffer of the given
            // length. MSG_DONTWAIT makes this non-blocking so an empty
            // socket returns immediately instead of stalling the
            // caller's poll loop.
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n <= 0 {
                return None;
            }
            if (n as usize) < MIN_ETHERNET_FRAME_LEN {
                tracing::warn!(
                    len = n,
                    min = MIN_ETHERNET_FRAME_LEN,
                    "dropping truncated datagram shorter than a minimum Ethernet frame"
                );
                continue;
            }
            if (n as usize) > MAX_FRAME_LEN {
                tracing::warn!(
                    len = n,
                    max = MAX_FRAME_LEN,
                    "dropping oversized datagram larger than the maximum Ethernet frame"
                );
                continue;
            }
            let frame = buf[..n as usize].to_vec();
            return Some((
                RawFdRxToken { frame },
                RawFdTxToken {
                    fd: self.fd.as_raw_fd(),
                },
            ));
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(RawFdTxToken {
            fd: self.fd.as_raw_fd(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME_LEN;
        caps
    }
}

/// Holds the frame `RawFdDevice::receive` already read off the fd; no
/// further I/O happens on `consume`.
pub struct RawFdRxToken {
    frame: Vec<u8>,
}

impl RxToken for RawFdRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

/// Writes the frame `f` builds straight onto the underlying fd when
/// consumed.
pub struct RawFdTxToken {
    fd: RawFd,
}

impl TxToken for RawFdTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        // SAFETY: self.fd is a valid open fd for the device's lifetime;
        // buf has exactly `len` initialized bytes to send. MSG_DONTWAIT
        // makes this non-blocking: without it, a guest that stops
        // draining its side of the socketpair fills the send buffer and
        // parks this call, and since it runs inside Stack::poll (a
        // synchronous call with no await to yield at), that would stall
        // the sandbox's whole network task rather than just this frame.
        let sent =
            unsafe { libc::send(self.fd, buf.as_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if sent < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                tracing::warn!(error = %err, "failed to write an Ethernet frame to the guest");
            }
            // WouldBlock (a full send buffer, i.e. the guest isn't
            // draining) is dropped silently: TCP retransmits or the
            // next protocol-level retry covers the loss, matching how
            // ingress already drops a datagram it can't use rather than
            // erroring the whole poll loop.
        } else if (sent as usize) != buf.len() {
            tracing::warn!(
                sent,
                expected = buf.len(),
                "short write sending an Ethernet frame to the guest"
            );
        }
        result
    }
}

/// Locally administered, unicast placeholder MAC for the interface.
/// Ward's guest reaches this over a socketpair rather than a real
/// Ethernet segment, so the address is never seen off-host; the locally
/// administered bit (0x02) keeps it out of any vendor's assigned range.
const INTERFACE_HARDWARE_ADDR: EthernetAddress =
    EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

/// The interface's own address, per the libslirp/QEMU user-networking
/// convention (10.0.2.0/24, gateway low in the range). Guest traffic
/// addressed here (DNS queries, ICMP echoes, DHCP requests) is accepted;
/// an interface with no assigned address only accepts broadcast traffic.
const INTERFACE_ADDR: IpAddress = IpAddress::v4(10, 0, 2, 2);
const INTERFACE_PREFIX_LEN: u8 = 24;

/// Port the guest-facing DNS relay socket listens on.
const DNS_SERVER_PORT: u16 = 53;
/// Bounds how many DNS datagrams (queries or the responses queued for
/// them) each direction of the relay socket can hold at once.
const DNS_SOCKET_BUFFER_PACKETS: usize = 8;
/// Bytes of payload storage in each direction of the relay socket, well
/// over the largest DNS-over-UDP message (512 bytes without EDNS0).
const DNS_SOCKET_BUFFER_BYTES: usize = 4096;

/// A DNS query relayed to the injected `Resolver`, awaiting its answer so
/// [`Stack::poll`] can send a response once it arrives.
struct PendingDnsQuery {
    transaction_id: u16,
    /// The query's own question section (name, type, class), copied
    /// verbatim into the response so the guest sees the question it asked.
    question: Vec<u8>,
    /// Domain name this query asked to resolve, kept so a resolved answer
    /// can be recorded against it in `Stack::resolved`.
    domain: String,
    remote: udp::UdpMetadata,
    answer: oneshot::Receiver<Vec<IpAddr>>,
}

/// A parsed guest DNS query, ready to be relayed to a [`Resolver`].
struct ParsedDnsQuery {
    transaction_id: u16,
    domain: String,
    question: Vec<u8>,
}

/// Parses `payload` (a UDP datagram's contents) as a DNS query, returning
/// its transaction ID, dotted-form queried name, and raw question section.
/// `None` for anything smoltcp's wire types can't parse as a question.
fn parse_dns_query(payload: &[u8]) -> Option<ParsedDnsQuery> {
    let packet = DnsPacket::new_checked(payload).ok()?;
    let (_, question) = DnsQuestion::parse(packet.payload()).ok()?;
    let domain = decode_dns_name(question.name)?;
    let question = packet.payload().get(..question.buffer_len())?.to_vec();
    Some(ParsedDnsQuery {
        transaction_id: packet.transaction_id(),
        domain,
        question,
    })
}

/// Decodes wire-format DNS labels (length-prefixed, zero-terminated) into
/// a dotted domain name.
fn decode_dns_name(raw: &[u8]) -> Option<String> {
    let mut labels = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        let len = raw[offset] as usize;
        if len == 0 {
            break;
        }
        offset += 1;
        let label = raw.get(offset..offset + len)?;
        labels.push(std::str::from_utf8(label).ok()?);
        offset += len;
    }
    Some(labels.join("."))
}

/// Compression pointer to offset 12 (0x000C), where the response's own
/// echoed question section starts, per RFC 1035 section 4.1.4.
const NAME_POINTER_TO_QUESTION: [u8; 2] = [0xC0, 0x0C];
/// TTL smoltcp's wire types have no opinion on; any positive value is
/// valid, so this picks a modest one rather than caching indefinitely.
const DNS_ANSWER_TTL_SECS: u32 = 60;

/// Upper bound on `Stack::resolved` entries. Guests can query arbitrarily
/// many names, so this caps memory use; the oldest entry is evicted (FIFO)
/// to make room for a new one once the cap is reached.
const MAX_RESOLVED_ENTRIES: usize = 4096;

/// Upper bound on `Stack::pending_dns_queries` entries. A guest can send
/// queries faster than the resolver answers them, so this caps memory use
/// the same way `MAX_RESOLVED_ENTRIES` caps `resolved`: the oldest
/// outstanding query is evicted (FIFO) to make room for a new one once the
/// cap is reached, on the assumption that a guest still waiting on a query
/// this old has likely already given up on it. Eviction just drops the
/// query's receiver; its paired resolver task will still run to
/// completion, and its `send` will simply fail with no observable effect.
const MAX_PENDING_DNS_QUERIES: usize = 4096;

/// Hand-encodes a DNS response: header, the query's own echoed question,
/// then one `A` record per IPv4 address in `answers` (IPv6 addresses are
/// dropped; there is no relay-side AAAA support yet). smoltcp's wire types
/// can parse a DNS response but not emit one, so this builds the bytes
/// directly per RFC 1035.
fn build_dns_response(transaction_id: u16, question: &[u8], answers: &[IpAddr]) -> Vec<u8> {
    let ipv4_answers: Vec<Ipv4Addr> = answers
        .iter()
        .filter_map(|addr| match addr {
            IpAddr::V4(v4) => Some(*v4),
            IpAddr::V6(_) => None,
        })
        .collect();

    let mut response = Vec::with_capacity(12 + question.len() + ipv4_answers.len() * 16);
    response.extend_from_slice(&transaction_id.to_be_bytes());
    let flags = DnsFlags::RESPONSE | DnsFlags::RECURSION_DESIRED | DnsFlags::RECURSION_AVAILABLE;
    response.extend_from_slice(&flags.bits().to_be_bytes());
    response.extend_from_slice(&1u16.to_be_bytes()); // question count
    response.extend_from_slice(&(ipv4_answers.len() as u16).to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes()); // authority count
    response.extend_from_slice(&0u16.to_be_bytes()); // additional count
    response.extend_from_slice(question);

    for addr in ipv4_answers {
        response.extend_from_slice(&NAME_POINTER_TO_QUESTION);
        response.extend_from_slice(&u16::from(DnsQueryType::A).to_be_bytes());
        response.extend_from_slice(&1u16.to_be_bytes()); // class IN
        response.extend_from_slice(&DNS_ANSWER_TTL_SECS.to_be_bytes());
        response.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        response.extend_from_slice(&addr.octets());
    }

    response
}

/// Address this `Stack` hands guests as the DHCP server identifier and
/// default gateway. Matches `INTERFACE_ADDR`'s octets, kept as a separate
/// `Ipv4Addr` constant since `wire::dhcpv4::Repr`'s fields need that
/// concrete type rather than the `IpAddress` enum.
const GATEWAY_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// Subnet mask matching `INTERFACE_PREFIX_LEN`'s /24.
const DHCP_SUBNET_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
/// First address the DHCP lease pool hands out, one above the gateway per
/// the libslirp/QEMU user-networking convention.
const DHCP_POOL_START: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// Number of addresses in the lease pool before it would wrap back to
/// `DHCP_POOL_START`; ample for the handful of guests one sandbox's
/// virtual NIC ever serves.
const DHCP_POOL_SIZE: u8 = 32;
/// Lease lifetime handed out in every OFFER, in seconds.
const DHCP_LEASE_DURATION_SECS: u32 = 3600;
/// Bounds how many DHCP datagrams the guest-facing DHCP server socket can
/// hold at once in each direction.
const DHCP_SOCKET_BUFFER_PACKETS: usize = 8;
/// Bytes of payload storage in each direction of the DHCP socket, well
/// over a DHCP message's fixed 236-byte header plus its options.
const DHCP_SOCKET_BUFFER_BYTES: usize = 1024;

/// Builds a DHCP OFFER answering `discover`: leases `your_ip` to the
/// requesting client and advertises this `Stack`'s own address as both the
/// DHCP server identifier and the default gateway. Unlike
/// `build_dns_response`, this reuses smoltcp's own `wire::dhcpv4::Repr::emit`
/// rather than a manual byte layout, since that wire type already covers
/// building a server reply and not just parsing one.
fn build_dhcp_offer(discover: &DhcpRepr, your_ip: Ipv4Addr) -> Vec<u8> {
    let dns_servers = heapless::Vec::from_slice(&[GATEWAY_ADDR])
        .expect("one DNS server address is well within the dns_servers option's fixed capacity");
    let offer = DhcpRepr {
        message_type: DhcpMessageType::Offer,
        transaction_id: discover.transaction_id,
        secs: 0,
        client_hardware_address: discover.client_hardware_address,
        client_ip: Ipv4Addr::UNSPECIFIED,
        your_ip,
        server_ip: GATEWAY_ADDR,
        router: Some(GATEWAY_ADDR),
        subnet_mask: Some(DHCP_SUBNET_MASK),
        relay_agent_ip: Ipv4Addr::UNSPECIFIED,
        broadcast: discover.broadcast,
        requested_ip: None,
        client_identifier: discover.client_identifier,
        server_identifier: Some(GATEWAY_ADDR),
        parameter_request_list: None,
        dns_servers: Some(dns_servers),
        max_size: None,
        lease_duration: Some(DHCP_LEASE_DURATION_SECS),
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };

    let mut buf = vec![0u8; offer.buffer_len()];
    let mut packet = DhcpPacket::new_unchecked(&mut buf[..]);
    offer
        .emit(&mut packet)
        .expect("offer repr should fit exactly in a buffer sized from its own buffer_len");
    buf
}

/// Owns a smoltcp `Interface`, the `SocketSet` it drives, and the
/// `RawFdDevice` backing both. `Interface::poll` takes the device by
/// `&mut` on every call, so `Stack` holds all three together instead of
/// exposing the device on its own.
pub struct Stack {
    device: RawFdDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
    resolver: Arc<dyn Resolver>,
    dns_socket_handle: SocketHandle,
    /// Queries relayed to the resolver, awaiting an answer. Capped at
    /// `MAX_PENDING_DNS_QUERIES` via `push_pending_dns_query`, oldest
    /// entry evicted first, since these arrive directly from
    /// guest-initiated queries with no other backpressure.
    pending_dns_queries: VecDeque<PendingDnsQuery>,
    dhcp_socket_handle: SocketHandle,
    /// Addresses leased so far, keyed by the requesting client's MAC, so a
    /// client that DISCOVERs again gets the same address back instead of
    /// consuming another slot in the pool.
    dhcp_leases: HashMap<EthernetAddress, Ipv4Addr>,
    /// IP addresses this `Stack` has seen resolved, keyed by the address a
    /// relayed DNS response answered with, mapped to the domain name that
    /// was queried for it. Populated as responses are relayed to the guest
    /// in `deliver_dns_answers`; see `resolved_domain_for`.
    resolved: HashMap<IpAddr, String>,
    /// Insertion order of `resolved`'s keys, oldest first, so the cap in
    /// `record_resolved` can evict FIFO instead of tracking recency.
    resolved_order: VecDeque<IpAddr>,
}

impl Stack {
    pub fn new(fd: OwnedFd, resolver: Box<dyn Resolver>) -> Stack {
        let mut device = RawFdDevice::new(fd);
        let config = Config::new(HardwareAddress::Ethernet(INTERFACE_HARDWARE_ADDR));
        let mut interface = Interface::new(config, &mut device, Instant::now());
        interface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(INTERFACE_ADDR, INTERFACE_PREFIX_LEN))
                .expect("a freshly created interface has room for its one static address");
        });

        // Vec-backed storage gives a SocketSet with no borrowed lifetime,
        // per smoltcp's own SocketSet doc comment.
        let mut sockets = SocketSet::new(Vec::new());
        let mut dns_socket = udp::Socket::new(
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; DNS_SOCKET_BUFFER_PACKETS],
                vec![0u8; DNS_SOCKET_BUFFER_BYTES],
            ),
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; DNS_SOCKET_BUFFER_PACKETS],
                vec![0u8; DNS_SOCKET_BUFFER_BYTES],
            ),
        );
        dns_socket
            .bind(DNS_SERVER_PORT)
            .expect("binding a freshly created UDP socket cannot fail");
        let dns_socket_handle = sockets.add(dns_socket);

        let mut dhcp_socket = udp::Socket::new(
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; DHCP_SOCKET_BUFFER_PACKETS],
                vec![0u8; DHCP_SOCKET_BUFFER_BYTES],
            ),
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; DHCP_SOCKET_BUFFER_PACKETS],
                vec![0u8; DHCP_SOCKET_BUFFER_BYTES],
            ),
        );
        dhcp_socket
            .bind(DHCP_SERVER_PORT)
            .expect("binding a freshly created UDP socket cannot fail");
        let dhcp_socket_handle = sockets.add(dhcp_socket);

        Stack {
            device,
            interface,
            sockets,
            resolver: Arc::from(resolver),
            dns_socket_handle,
            pending_dns_queries: VecDeque::new(),
            dhcp_socket_handle,
            dhcp_leases: HashMap::new(),
            resolved: HashMap::new(),
            resolved_order: VecDeque::new(),
        }
    }

    /// Domain name a relayed DNS response most recently resolved `ip` to,
    /// or `None` if this `Stack` has not recorded such a response.
    pub fn resolved_domain_for(&self, ip: IpAddr) -> Option<&str> {
        self.resolved.get(&ip).map(String::as_str)
    }

    /// Records that `domain` resolved to `ip` in a response just relayed to
    /// the guest. Evicts the oldest entry first once `MAX_RESOLVED_ENTRIES`
    /// is reached; re-recording an address already present updates its
    /// domain in place without changing its eviction order.
    fn record_resolved(&mut self, ip: IpAddr, domain: String) {
        if !self.resolved.contains_key(&ip) {
            if self.resolved_order.len() >= MAX_RESOLVED_ENTRIES
                && let Some(oldest) = self.resolved_order.pop_front()
            {
                self.resolved.remove(&oldest);
            }
            self.resolved_order.push_back(ip);
        }
        self.resolved.insert(ip, domain);
    }

    /// Test-only entry point into the resolved-map eviction logic. Available
    /// in all builds so integration-test harnesses in `tests/` can use it
    /// without a `#[cfg(test)]` restriction; production callers reach
    /// `record_resolved` only via a relayed DNS response.
    pub fn record_resolved_for_test(&mut self, ip: IpAddr, domain: &str) {
        self.record_resolved(ip, domain.to_string());
    }

    /// Queues `query` as awaiting a resolver answer. Evicts the oldest
    /// outstanding query first once `MAX_PENDING_DNS_QUERIES` is reached;
    /// see that constant for why eviction (rather than rejecting the new
    /// query) is the right tradeoff here.
    fn push_pending_dns_query(&mut self, query: PendingDnsQuery) {
        if self.pending_dns_queries.len() >= MAX_PENDING_DNS_QUERIES {
            self.pending_dns_queries.pop_front();
        }
        self.pending_dns_queries.push_back(query);
    }

    /// Test-only entry point into the pending-query-table eviction logic,
    /// constructing a query whose answer never arrives (mirroring a guest
    /// that floods queries and never gets a response) without driving a
    /// real frame through the socketpair. The paired sender is leaked
    /// rather than dropped, so the receiver never reports closed for the
    /// duration of the test.
    pub fn push_pending_dns_query_for_test(&mut self, transaction_id: u16, domain: &str) {
        let (answer_tx, answer_rx) = oneshot::channel();
        std::mem::forget(answer_tx);
        self.push_pending_dns_query(PendingDnsQuery {
            transaction_id,
            question: Vec::new(),
            domain: domain.to_string(),
            remote: udp::UdpMetadata::from(IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED),
                0,
            )),
            answer: answer_rx,
        });
    }

    /// Number of DNS queries currently awaiting a resolver answer.
    pub fn pending_dns_query_count_for_test(&self) -> usize {
        self.pending_dns_queries.len()
    }

    /// Processes pending ingress on the device and flushes queued
    /// egress. Returns smoltcp's own `PollResult`: `SocketStateChanged`
    /// when a caller should recheck socket state, `None` otherwise.
    pub fn poll(&mut self) -> PollResult {
        let result = self
            .interface
            .poll(Instant::now(), &mut self.device, &mut self.sockets);
        self.relay_dns_queries();
        self.deliver_dns_answers();
        self.serve_dhcp();
        result
    }

    /// Returns the address leased to `mac`, assigning the next free
    /// address from the pool on a client's first DISCOVER and returning
    /// that same address on every later one instead of consuming another
    /// slot.
    fn lease_for(&mut self, mac: EthernetAddress) -> Ipv4Addr {
        if let Some(addr) = self.dhcp_leases.get(&mac) {
            return *addr;
        }
        let offset = (self.dhcp_leases.len() as u8) % DHCP_POOL_SIZE;
        let [a, b, c, d] = DHCP_POOL_START.octets();
        let addr = Ipv4Addr::new(a, b, c, d + offset);
        self.dhcp_leases.insert(mac, addr);
        addr
    }

    /// Drains every guest datagram waiting on the DHCP server socket,
    /// answering each DISCOVER with an OFFER. smoltcp's `socket::dhcpv4`
    /// is a DHCP client only, so this hand-rolls the server side on top of
    /// a plain UDP socket bound to port 67.
    fn serve_dhcp(&mut self) {
        loop {
            let payload = {
                let socket = self.sockets.get_mut::<udp::Socket>(self.dhcp_socket_handle);
                match socket.recv() {
                    Ok((data, _meta)) => data.to_vec(),
                    Err(_) => break,
                }
            };
            let Ok(packet) = DhcpPacket::new_checked(payload.as_slice()) else {
                tracing::warn!("dropping malformed guest DHCP datagram on UDP:67");
                continue;
            };
            let Ok(discover) = DhcpRepr::parse(&packet) else {
                tracing::warn!("dropping unparseable guest DHCP datagram on UDP:67");
                continue;
            };
            if discover.message_type != DhcpMessageType::Discover {
                // REQUEST/ACK handling is not implemented yet.
                continue;
            }
            let offered_ip = self.lease_for(discover.client_hardware_address);
            let offer = build_dhcp_offer(&discover, offered_ip);
            // The client has no address yet, so the offer is broadcast
            // back rather than sent to the (still unspecified) source
            // address the DISCOVER arrived from.
            let reply_to = udp::UdpMetadata::from(IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Addr::BROADCAST),
                DHCP_CLIENT_PORT,
            ));
            let socket = self.sockets.get_mut::<udp::Socket>(self.dhcp_socket_handle);
            if let Err(err) = socket.send_slice(&offer, reply_to) {
                tracing::warn!(error = %err, "failed to queue guest DHCP offer");
            }
        }
    }

    /// Drains every guest datagram waiting on the DNS relay socket,
    /// spawning a resolver call for each one it can parse as a query.
    fn relay_dns_queries(&mut self) {
        loop {
            let (payload, remote) = {
                let socket = self.sockets.get_mut::<udp::Socket>(self.dns_socket_handle);
                match socket.recv() {
                    Ok((data, meta)) => (data.to_vec(), meta),
                    Err(_) => break,
                }
            };
            let Some(ParsedDnsQuery {
                transaction_id,
                domain,
                question,
            }) = parse_dns_query(&payload)
            else {
                tracing::warn!("dropping malformed guest DNS query on UDP:53");
                continue;
            };
            let (answer_tx, answer_rx) = oneshot::channel();
            let resolver = Arc::clone(&self.resolver);
            let domain_for_resolve = domain.clone();
            tokio::task::spawn(async move {
                let answers = resolver.resolve(&domain_for_resolve).await;
                // Best-effort: a dropped receiver means this Stack (and
                // its pending-query list) has already gone away.
                let _ = answer_tx.send(answers);
            });
            self.push_pending_dns_query(PendingDnsQuery {
                transaction_id,
                question,
                domain,
                remote,
                answer: answer_rx,
            });
        }
    }

    /// Checks every in-flight resolver call and queues a response for each
    /// one that has answered, leaving the rest pending for a later tick.
    fn deliver_dns_answers(&mut self) {
        let mut still_pending = VecDeque::new();
        for mut query in std::mem::take(&mut self.pending_dns_queries) {
            match query.answer.try_recv() {
                Ok(answers) => {
                    let response =
                        build_dns_response(query.transaction_id, &query.question, &answers);
                    // Only IPv4 answers are ever sent to the guest (see
                    // build_dns_response), so only those are recorded as
                    // resolved; an IPv6 answer the guest never saw would be
                    // a false entry in the map.
                    for addr in answers.iter().filter(|addr| matches!(addr, IpAddr::V4(_))) {
                        self.record_resolved(*addr, query.domain.clone());
                    }
                    let socket = self.sockets.get_mut::<udp::Socket>(self.dns_socket_handle);
                    if let Err(err) = socket.send_slice(&response, query.remote) {
                        tracing::warn!(error = %err, "failed to queue guest DNS response");
                    }
                }
                Err(oneshot::error::TryRecvError::Empty) => still_pending.push_back(query),
                Err(oneshot::error::TryRecvError::Closed) => {
                    tracing::warn!(
                        "DNS resolver task ended without answering a guest query; dropping it"
                    );
                }
            }
        }
        self.pending_dns_queries = still_pending;
    }
}

/// Commands a caller sends to a spawned sandbox's network task over its
/// `cmd_tx` channel. This is how the task is controlled from outside
/// without ever locking its `RawFdDevice`.
pub enum StackCommand {
    /// Tells the task to exit its poll loop and return.
    Shutdown,
}

/// Returned by [`spawn_for_sandbox`]. Holds the guest-side fd to hand to
/// the VMM, the task's `JoinHandle`, and the command channel to control
/// it. Deliberately does not hold the device itself: that lives
/// exclusively inside the spawned task, so nothing about polling this
/// sandbox's network ever requires locking a shared sandbox map.
pub struct SmoltcpHandle {
    pub guest_fd: RawFd,
    pub task: JoinHandle<()>,
    pub cmd_tx: mpsc::Sender<StackCommand>,
}

impl SmoltcpHandle {
    /// Sends [`StackCommand::Shutdown`] to the spawned task and awaits its
    /// join. Idempotent: a task that has already finished (from a prior
    /// `detach` call) is detected via `is_finished` and this returns
    /// immediately, and a `cmd_tx` send on an already-closed channel is a
    /// non-error no-op rather than a failure, mirroring
    /// [`crate::passt::PasstHandle::kill`]'s idempotent shape.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Process`] if the task panicked instead of
    /// returning normally.
    pub async fn detach(&mut self) -> Result<(), Error> {
        if self.task.is_finished() {
            return Ok(());
        }
        // A closed channel means the task already stopped reading
        // commands (or a previous detach already sent Shutdown); either
        // way there is nothing left to signal.
        let _ = self.cmd_tx.send(StackCommand::Shutdown).await;
        // Poll by reference (JoinHandle is Unpin) instead of consuming
        // self.task, so the handle stays usable if detach is called
        // again.
        (&mut self.task)
            .await
            .map_err(|e| Error::Process(format!("smoltcp task join failed: {e}")))
    }
}

/// How long the task waits for a command before polling the device
/// again when nothing has arrived. Frame processing beyond draining the
/// socket is future work; this cadence only bounds how promptly a
/// `Shutdown` not already caught by the `select!` race is noticed.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Allocates an `AF_UNIX SOCK_DGRAM` socketpair for `sandbox_id` and
/// spawns a dedicated task that owns the host-side end exclusively via
/// a `RawFdDevice`, polling it on a schedule and listening for commands.
/// The returned handle carries only the guest-side fd, the task's
/// `JoinHandle`, and a `Sender` to control it.
///
/// `sandbox_id` and `opts` aren't consumed yet (no port-forwarding or
/// per-sandbox identification is wired up); they are accepted now to
/// match the shape callers will need once that lands.
///
/// # Errors
///
/// Returns [`Error::Process`] if the socketpair syscall fails.
pub async fn spawn_for_sandbox(
    _sandbox_id: &str,
    _opts: &AttachOptions,
) -> Result<SmoltcpHandle, Error> {
    // socketpair(AF_UNIX, SOCK_DGRAM, 0) → [host_fd, guest_fd]. SOCK_DGRAM
    // (unlike passt's SOCK_STREAM) preserves datagram boundaries, matching
    // RawFdDevice's one-recv-per-frame reads.
    // SAFETY: socketpair is a pure syscall with no preconditions beyond a
    // valid `sv` pointer; both fds are closed on error via OwnedFd/drop.
    let mut sv: [std::ffi::c_int; 2] = [-1, -1];
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) };
    if ret != 0 {
        return Err(Error::Process(format!(
            "socketpair(AF_UNIX, SOCK_DGRAM) failed: errno {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: socketpair succeeded; sv[0] and sv[1] are valid open fds.
    let host_fd = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let guest_fd: RawFd = sv[1];

    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    let task = tokio::task::spawn(async move {
        // Stack (device + Interface + SocketSet) lives only inside this
        // task, so the host-side fd is never shared or locked from
        // outside it.
        let mut stack = Stack::new(host_fd, Box::new(SystemResolver));
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(StackCommand::Shutdown) | None => break,
                },
                () = tokio::time::sleep(POLL_INTERVAL) => {
                    // Drives ingress/egress for one tick; routing polled
                    // sockets to sandbox-visible state is future work.
                    let _ = stack.poll();
                }
            }
        }
    });

    Ok(SmoltcpHandle {
        guest_fd,
        task,
        cmd_tx,
    })
}

#[derive(Debug, Default)]
pub struct SmoltcpBackend;

#[async_trait::async_trait]
impl NetworkBackend for SmoltcpBackend {
    fn name(&self) -> &'static str {
        "smoltcp"
    }

    async fn probe(&self) -> Result<(), Error> {
        // smoltcp is in-process; nothing to probe. We do compile-check
        // that smoltcp's types are reachable so a future feature drift
        // surfaces at the right boundary.
        let _ = std::mem::size_of::<smoltcp::wire::IpAddress>();
        Ok(())
    }

    async fn attach(&self, _sandbox_id: &str, _opts: &AttachOptions) -> Result<AttachId, Error> {
        Err(Error::Unimplemented(
            "smoltcp backend: see docs/adr/018-rootless-networking.md \
             'Future work' for the planned implementation. Use \
             WARD_NETWORK_BACKEND=passt for now."
                .into(),
        ))
    }

    async fn detach(&self, _attach_id: &AttachId) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn given_scaffold_when_probe_then_ok() {
        SmoltcpBackend.probe().await.unwrap();
    }

    #[tokio::test]
    async fn given_scaffold_when_attach_then_unimplemented() {
        let err = SmoltcpBackend
            .attach("sb", &AttachOptions::default())
            .await
            .unwrap_err();
        match err {
            Error::Unimplemented(msg) => assert!(msg.contains("018")),
            other => panic!("expected Unimplemented, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn given_spawn_for_sandbox_when_called_then_returns_valid_guest_fd() {
        let result = spawn_for_sandbox("test-sandbox-id", &AttachOptions::default()).await;
        match result {
            Ok(SmoltcpHandle { guest_fd, .. }) => assert!(guest_fd >= 0),
            Err(err) => panic!("expected Ok(SmoltcpHandle), got Err({err:?})"),
        }
    }

    #[tokio::test]
    async fn given_spawned_task_when_shutdown_sent_then_task_joins_cleanly() {
        let handle = spawn_for_sandbox("test-sandbox-id", &AttachOptions::default())
            .await
            .expect("spawn_for_sandbox should succeed");
        // Send Shutdown immediately, before any POLL_INTERVAL tick could
        // have elapsed, so a prompt join here can only be explained by
        // the task's select! racing cmd_rx.recv() rather than waiting for
        // its next scheduled poll.
        let start = std::time::Instant::now();
        handle
            .cmd_tx
            .send(StackCommand::Shutdown)
            .await
            .expect("cmd channel should still be open");
        handle
            .task
            .await
            .expect("task should join without panicking");
        let elapsed = start.elapsed();
        assert!(
            elapsed < POLL_INTERVAL,
            "join should be prompt (elapsed {elapsed:?} should be well under \
             POLL_INTERVAL {POLL_INTERVAL:?}), proving select! races the \
             command receive rather than waiting for the next poll tick"
        );
    }

    #[tokio::test]
    async fn given_spawned_task_when_stack_polls_then_shuts_down_cleanly() {
        let handle = spawn_for_sandbox("test-sandbox-id", &AttachOptions::default())
            .await
            .expect("spawn_for_sandbox should succeed");
        // Outlive at least one POLL_INTERVAL tick so the task's loop
        // drives Stack::poll before shutdown; a panic there would fail
        // the join below instead of this sleep.
        tokio::time::sleep(POLL_INTERVAL * 2).await;
        handle
            .cmd_tx
            .send(StackCommand::Shutdown)
            .await
            .expect("cmd channel should still be open");
        handle
            .task
            .await
            .expect("task should join without panicking");
    }

    #[tokio::test]
    async fn given_spawn_then_detach_when_detach_again_then_idempotent() {
        let mut handle = spawn_for_sandbox("test-sandbox-id", &AttachOptions::default())
            .await
            .expect("spawn_for_sandbox should succeed");
        handle.detach().await.expect("first detach should succeed");
        handle
            .detach()
            .await
            .expect("second detach should succeed (idempotent)");
    }
}
