// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Bytes that cross a sandbox's virtual network device.

use std::sync::atomic::{AtomicU64, Ordering};

/// Totals at one instant, from the guest's point of view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetBytes {
    /// Ethernet frame bytes the guest received.
    pub rx: u64,
    /// Ethernet frame bytes the guest sent.
    pub tx: u64,
}

/// Lock-free byte counters shared between the network task and readers.
#[derive(Debug, Default)]
pub struct NetCounters {
    rx: AtomicU64,
    tx: AtomicU64,
}

impl NetCounters {
    /// Count a frame delivered to the guest.
    pub fn add_rx(&self, bytes: usize) {
        self.rx.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Count a frame received from the guest.
    pub fn add_tx(&self, bytes: usize) {
        self.tx.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> NetBytes {
        NetBytes {
            rx: self.rx.load(Ordering::Relaxed),
            tx: self.tx.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_frames_in_both_directions_when_snapshot_then_each_direction_is_summed() {
        // Arrange
        let counters = NetCounters::default();

        // Act
        counters.add_rx(100);
        counters.add_rx(50);
        counters.add_tx(7);

        // Assert
        assert_eq!(counters.snapshot(), NetBytes { rx: 150, tx: 7 });
    }
}
