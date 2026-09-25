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

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

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
        // Sized one byte past MAX_FRAME_LEN so an oversized datagram (the
        // kernel silently truncates SOCK_DGRAM reads to the buffer size)
        // fills the whole buffer and is distinguishable from a frame that
        // legitimately fills exactly MAX_FRAME_LEN bytes.
        let mut buf = [0u8; MAX_FRAME_LEN + 1];
        // SAFETY: self.fd is a valid open fd for the device's lifetime;
        // buf is a valid, initialized buffer of the given length.
        // MSG_DONTWAIT makes this non-blocking so an empty socket returns
        // immediately instead of stalling the caller's poll loop.
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
            return None;
        }
        if (n as usize) > MAX_FRAME_LEN {
            tracing::warn!(
                len = n,
                max = MAX_FRAME_LEN,
                "dropping oversized datagram larger than the maximum Ethernet frame"
            );
            return None;
        }
        let frame = buf[..n as usize].to_vec();
        Some((
            RawFdRxToken { frame },
            RawFdTxToken {
                fd: self.fd.as_raw_fd(),
            },
        ))
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
        // buf has exactly `len` initialized bytes for f to have written.
        unsafe {
            libc::write(self.fd, buf.as_ptr().cast(), buf.len());
        }
        result
    }
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
}
