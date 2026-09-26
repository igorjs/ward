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

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

use smoltcp::iface::{Config, Interface, PollResult, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{AttachId, AttachOptions, Error, NetworkBackend};

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

/// Owns a smoltcp `Interface`, the `SocketSet` it drives, and the
/// `RawFdDevice` backing both. `Interface::poll` takes the device by
/// `&mut` on every call, so `Stack` holds all three together instead of
/// exposing the device on its own.
pub struct Stack {
    device: RawFdDevice,
    interface: Interface,
    sockets: SocketSet<'static>,
}

impl Stack {
    pub fn new(fd: OwnedFd) -> Stack {
        let mut device = RawFdDevice::new(fd);
        let config = Config::new(HardwareAddress::Ethernet(INTERFACE_HARDWARE_ADDR));
        let interface = Interface::new(config, &mut device, Instant::now());
        // Vec-backed storage gives a SocketSet with no borrowed lifetime,
        // per smoltcp's own SocketSet doc comment.
        let sockets = SocketSet::new(Vec::new());
        Stack {
            device,
            interface,
            sockets,
        }
    }

    /// Processes pending ingress on the device and flushes queued
    /// egress. Returns smoltcp's own `PollResult`: `SocketStateChanged`
    /// when a caller should recheck socket state, `None` otherwise.
    pub fn poll(&mut self) -> PollResult {
        self.interface
            .poll(Instant::now(), &mut self.device, &mut self.sockets)
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
        let mut stack = Stack::new(host_fd);
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
