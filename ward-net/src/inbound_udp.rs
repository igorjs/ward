// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host-to-guest UDP port forwarding.
//!
//! A host `UdpSocket` receives datagrams and hands each one to the smoltcp
//! task. The task keeps one flow per host peer: a UDP socket on a local
//! gateway port that sends the payload to the guest and returns the guest's
//! replies to that peer. A flow ends after it has been idle for a while.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::Error;
use crate::inbound::InboundHandle;
use crate::smoltcp_backend::StackCommand;

/// Concurrent peers one sandbox network relays over UDP. A datagram from a
/// new peer past this is dropped, so a flood cannot grow the flow table.
pub const MAX_UDP_FLOWS: usize = 256;

/// How long a flow may sit idle before its socket is freed.
pub const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest datagram the host socket reads.
const MAX_DATAGRAM_BYTES: usize = 65535;

/// Datagrams a flow queues in each direction.
const FLOW_PACKETS: usize = 16;

/// Bytes a flow queues in each direction.
const FLOW_BUFFER_BYTES: usize = 32 * 1024;

/// Pause after a failed receive so the listener does not spin.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// A datagram from a host peer on its way to the smoltcp task.
pub struct NewDatagram {
    host: Arc<UdpSocket>,
    peer: SocketAddr,
    guest_port: u16,
    payload: Vec<u8>,
    alive: Weak<()>,
}

/// One host peer talking to one guest port.
pub(crate) struct UdpFlow {
    handle: SocketHandle,
    local_port: u16,
    host: Arc<UdpSocket>,
    peer: SocketAddr,
    guest_port: u16,
    last_activity: Instant,
    alive: Weak<()>,
}

/// Binds `bind` and starts forwarding its datagrams to `guest_port` through
/// `cmd_tx`.
pub(crate) async fn start_udp_listener(
    bind: SocketAddr,
    guest_port: u16,
    cmd_tx: mpsc::Sender<StackCommand>,
) -> Result<InboundHandle, Error> {
    if !bind.ip().is_loopback() {
        tracing::warn!(address = %bind, "publishing a guest UDP port on a non-loopback address");
    }
    let socket = UdpSocket::bind(bind)
        .await
        .map_err(|source| Error::Listen { addr: bind, source })?;
    let local_addr = socket
        .local_addr()
        .map_err(|err| Error::Process(format!("cannot read socket address: {err}")))?;
    let host = Arc::new(socket);
    let alive = Arc::new(());
    let weak = Arc::downgrade(&alive);
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM_BYTES];
        loop {
            let (len, peer) = match host.recv_from(&mut buf).await {
                Ok(received) => received,
                Err(err) => {
                    tracing::warn!(error = %err, "receive failed on a published UDP port");
                    tokio::time::sleep(RECV_ERROR_BACKOFF).await;
                    continue;
                }
            };
            let datagram = NewDatagram {
                host: Arc::clone(&host),
                peer,
                guest_port,
                payload: buf[..len].to_vec(),
                alive: weak.clone(),
            };
            if cmd_tx
                .send(StackCommand::InboundUdp(datagram))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(InboundHandle::new(local_addr, task, alive))
}

impl NewDatagram {
    pub(crate) fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

/// A fresh UDP socket for a flow.
pub(crate) fn new_socket() -> udp::Socket<'static> {
    let buffer = || {
        udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; FLOW_PACKETS],
            vec![0u8; FLOW_BUFFER_BYTES],
        )
    };
    udp::Socket::new(buffer(), buffer())
}

impl UdpFlow {
    pub(crate) fn new(handle: SocketHandle, local_port: u16, first: &NewDatagram) -> Self {
        UdpFlow {
            handle,
            local_port,
            host: Arc::clone(&first.host),
            peer: first.peer,
            guest_port: first.guest_port,
            last_activity: Instant::now(),
            alive: first.alive.clone(),
        }
    }

    pub(crate) fn handle(&self) -> SocketHandle {
        self.handle
    }

    pub(crate) fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Whether `datagram` belongs to this flow.
    pub(crate) fn serves(&self, datagram: &NewDatagram) -> bool {
        self.peer == datagram.peer
            && self.guest_port == datagram.guest_port
            && Arc::ptr_eq(&self.host, &datagram.host)
    }

    /// Sends `payload` to the guest from this flow's gateway port.
    pub(crate) fn send_to_guest(
        &mut self,
        socket: &mut udp::Socket,
        guest: IpAddress,
        payload: &[u8],
    ) -> bool {
        self.last_activity = Instant::now();
        socket
            .send_slice(payload, IpEndpoint::new(guest, self.guest_port))
            .is_ok()
    }

    /// Relays the guest's replies to the peer. Returns true once the flow is
    /// over: idle too long, or its listener is gone.
    pub(crate) fn pump(&mut self, socket: &mut udp::Socket) -> bool {
        while let Ok((payload, _from)) = socket.recv() {
            self.last_activity = Instant::now();
            if let Err(err) = self.host.try_send_to(payload, self.peer) {
                tracing::debug!(peer = %self.peer, error = %err, "cannot return a UDP reply");
            }
        }
        self.alive.strong_count() == 0 || self.last_activity.elapsed() >= UDP_IDLE_TIMEOUT
    }
}
