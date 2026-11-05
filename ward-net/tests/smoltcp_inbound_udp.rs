// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host-to-guest UDP forwarding tests. The guest is a second smoltcp stack on
//! the far end of the sandbox's socketpair.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::pin::Pin;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{dhcpv4, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use ward_net::AttachOptions;
use ward_net::smoltcp_backend::{RawFdDevice, SmoltcpHandle, spawn_for_sandbox};

const GUEST_MAC: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
const GUEST_ADDR: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GUEST_PORT: u16 = 5353;
const GUEST_TICK: Duration = Duration::from_millis(2);
const PACKET_SLOTS: usize = 8;
const PACKET_BYTES: usize = 2048;
const DEADLINE: Duration = Duration::from_secs(20);
const POLL_STEP: Duration = Duration::from_millis(10);
const SILENCE: Duration = Duration::from_millis(500);

fn always_allow(_domain: &str, _port: u16) -> Pin<Box<dyn Future<Output = bool> + Send>> {
    Box::pin(async { true })
}

async fn wait_for_lease(handle: &SmoltcpHandle) {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while handle.guest_addr().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a lease"
        );
        tokio::time::sleep(POLL_STEP).await;
    }
}

/// A guest that echoes each datagram on `GUEST_PORT` with a prefix.
fn spawn_echo_guest(fd: i32) -> JoinHandle<()> {
    // SAFETY: the fd comes from the handle's socketpair and is owned here.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    tokio::spawn(async move {
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
        let buffer = || {
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; PACKET_SLOTS],
                vec![0u8; PACKET_SLOTS * PACKET_BYTES],
            )
        };
        let handle = sockets.add(udp::Socket::new(buffer(), buffer()));
        sockets
            .get_mut::<udp::Socket>(handle)
            .bind(GUEST_PORT)
            .unwrap();
        loop {
            iface.poll(Instant::now(), &mut device, &mut sockets);
            let socket = sockets.get_mut::<udp::Socket>(handle);
            while let Ok((data, meta)) = socket.recv() {
                let mut reply = b"echo:".to_vec();
                reply.extend_from_slice(data);
                let _ = socket.send_slice(&reply, meta.endpoint);
            }
            tokio::time::sleep(GUEST_TICK).await;
        }
    })
}

async fn published() -> (SmoltcpHandle, SocketAddr, JoinHandle<()>, impl Drop) {
    let handle = spawn_for_sandbox(
        "udp-test",
        &AttachOptions::default(),
        Box::new(always_allow),
    )
    .await
    .expect("spawn_for_sandbox");
    let guest = spawn_echo_guest(handle.guest_fd);
    wait_for_lease(&handle).await;
    let forward = handle
        .add_inbound_udp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), GUEST_PORT)
        .await
        .unwrap();
    (handle, forward.local_addr(), guest, forward)
}

async fn ask(client: &UdpSocket, target: SocketAddr, body: &[u8]) -> Vec<u8> {
    client.send_to(body, target).await.unwrap();
    let mut buf = [0u8; PACKET_BYTES];
    let (len, from) = tokio::time::timeout(DEADLINE, client.recv_from(&mut buf))
        .await
        .expect("reply timed out")
        .unwrap();
    assert_eq!(from, target);
    buf[..len].to_vec()
}

#[tokio::test]
async fn given_a_guest_udp_server_when_the_host_sends_a_datagram_then_the_reply_comes_back() {
    // Arrange
    let (_handle, target, guest, _forward) = published().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Act
    let reply = ask(&client, target, b"ping").await;

    // Assert
    assert_eq!(reply, b"echo:ping");
    guest.abort();
}

#[tokio::test]
async fn given_two_host_peers_when_each_sends_then_each_gets_its_own_reply() {
    // Arrange
    let (_handle, target, guest, _forward) = published().await;
    let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Act
    let one = ask(&first, target, b"one").await;
    let two = ask(&second, target, b"two").await;
    let again = ask(&first, target, b"again").await;

    // Assert
    assert_eq!(one, b"echo:one");
    assert_eq!(two, b"echo:two");
    assert_eq!(again, b"echo:again");
    guest.abort();
}

#[tokio::test]
async fn given_a_dropped_handle_when_a_datagram_is_sent_then_nothing_comes_back() {
    // Arrange
    let (_handle, target, guest, forward) = published().await;
    drop(forward);
    tokio::time::sleep(SILENCE).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Act
    client.send_to(b"ping", target).await.unwrap();
    let mut buf = [0u8; PACKET_BYTES];
    let reply = tokio::time::timeout(SILENCE, client.recv_from(&mut buf)).await;

    // Assert
    assert!(reply.is_err(), "a closed port must stay silent");
    guest.abort();
}

#[tokio::test]
async fn given_a_datagram_over_the_mtu_when_sent_then_it_is_dropped_and_small_ones_still_work() {
    // Arrange
    let (_handle, target, guest, _forward) = published().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let oversized = vec![7u8; 1473];

    // Act
    client.send_to(&oversized, target).await.unwrap();
    let mut buf = [0u8; PACKET_BYTES];
    let silent = tokio::time::timeout(SILENCE, client.recv_from(&mut buf)).await;
    let reply = ask(&client, target, b"ping").await;

    // Assert
    assert!(silent.is_err(), "an oversized datagram must get no reply");
    assert_eq!(reply, b"echo:ping");
    guest.abort();
}

#[tokio::test]
async fn given_one_host_with_many_source_ports_when_it_sends_then_only_the_per_host_cap_is_served()
{
    // Arrange
    let (_handle, target, guest, _forward) = published().await;
    let over_cap = ward_net::inbound_udp::MAX_UDP_FLOWS_PER_HOST + 1;
    let mut clients = Vec::new();
    for _ in 0..over_cap {
        clients.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    }

    // Act
    let mut answered = 0;
    for client in &clients {
        client.send_to(b"ping", target).await.unwrap();
        let mut buf = [0u8; PACKET_BYTES];
        if tokio::time::timeout(SILENCE, client.recv_from(&mut buf))
            .await
            .is_ok()
        {
            answered += 1;
        }
    }

    // Assert
    assert_eq!(answered, ward_net::inbound_udp::MAX_UDP_FLOWS_PER_HOST);
    guest.abort();
}
