// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Bounded record of the destinations a sandbox's egress policy refused.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Most distinct hosts remembered per sandbox. A guest can probe unlimited
/// names, so the least recently seen host makes room for a new one.
pub const MAX_BLOCKED_HOSTS: usize = 256;

/// One refused destination: a DNS name, or an IP when no name is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedDestination {
    pub host: String,
    /// How many times the policy refused it.
    pub count: u64,
    pub last_seen: SystemTime,
}

/// Per-sandbox recorder of refused destinations, deduplicated by host.
#[derive(Debug, Default)]
pub struct BlockedEgress {
    hosts: Mutex<HashMap<String, BlockedDestination>>,
}

impl BlockedEgress {
    /// Notes a refusal of `host` at `now`. Names compare case-insensitively.
    pub fn record(&self, host: &str, now: SystemTime) {
        let host = host.to_ascii_lowercase();
        let mut hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seen) = hosts.get_mut(&host) {
            seen.count += 1;
            seen.last_seen = now;
            return;
        }
        if hosts.len() >= MAX_BLOCKED_HOSTS
            && let Some(oldest) = hosts
                .values()
                .min_by_key(|d| d.last_seen)
                .map(|d| d.host.clone())
        {
            hosts.remove(&oldest);
        }
        hosts.insert(
            host.clone(),
            BlockedDestination {
                host,
                count: 1,
                last_seen: now,
            },
        );
    }

    /// Every remembered host, most recently seen first, then by name.
    pub fn snapshot(&self) -> Vec<BlockedDestination> {
        let hosts = self.hosts.lock().unwrap_or_else(|e| e.into_inner());
        let mut all: Vec<_> = hosts.values().cloned().collect();
        all.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then(a.host.cmp(&b.host)));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn given_repeated_host_when_recorded_then_counts_and_keeps_last_seen() {
        // Arrange
        let rec = BlockedEgress::default();

        // Act
        rec.record("Example.com", at(1));
        rec.record("example.com", at(5));

        // Assert
        let all = rec.snapshot();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].host, "example.com");
        assert_eq!(all[0].count, 2);
        assert_eq!(all[0].last_seen, at(5));
    }

    #[test]
    fn given_two_hosts_when_snapshot_then_most_recent_first() {
        // Arrange
        let rec = BlockedEgress::default();
        rec.record("a.test", at(1));
        rec.record("b.test", at(2));

        // Act
        let hosts: Vec<_> = rec.snapshot().into_iter().map(|d| d.host).collect();

        // Assert
        assert_eq!(hosts, ["b.test", "a.test"]);
    }

    #[test]
    fn given_full_recorder_when_new_host_then_evicts_least_recent() {
        // Arrange
        let rec = BlockedEgress::default();
        for i in 0..MAX_BLOCKED_HOSTS {
            rec.record(&format!("h{i}.test"), at(i as u64 + 1));
        }

        // Act
        rec.record("new.test", at(10_000));

        // Assert
        let all = rec.snapshot();
        assert_eq!(all.len(), MAX_BLOCKED_HOSTS);
        assert!(all.iter().any(|d| d.host == "new.test"));
        assert!(!all.iter().any(|d| d.host == "h0.test"));
    }

    #[test]
    fn given_ip_label_when_recorded_then_kept_as_is() {
        // Arrange
        let rec = BlockedEgress::default();

        // Act
        rec.record("203.0.113.9", at(1));

        // Assert
        assert_eq!(rec.snapshot()[0].host, "203.0.113.9");
    }
}
