// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Per-sandbox egress proxy with domain allowlist enforcement.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;

use crate::protocol::EgressPolicy;

// ---------------------------------------------------------------------------
// Log entry
// ---------------------------------------------------------------------------

/// A single egress connection attempt recorded by the proxy.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub sandbox_id: String,
    pub domain: String,
    pub port: u16,
    pub allowed: bool,
    pub timestamp: SystemTime,
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// Per-sandbox TCP egress proxy.
///
/// Intercepts outbound connections, checks them against the configured
/// `EgressPolicy`, logs the decision, and either allows or rejects them.
#[derive(Debug)]
pub struct EgressProxy {
    sandbox_id: String,
    policy: EgressPolicy,
    log: Arc<RwLock<VecDeque<LogEntry>>>,
    /// Maximum number of log entries retained in memory.
    log_capacity: usize,
}

impl EgressProxy {
    pub fn new(sandbox_id: String, policy: EgressPolicy) -> Self {
        Self {
            sandbox_id,
            policy,
            log: Arc::new(RwLock::new(VecDeque::new())),
            log_capacity: 1_000,
        }
    }

    /// Evaluate whether a connection to `domain:port` is permitted.
    ///
    /// Records the decision to the in-memory log regardless of outcome.
    pub async fn check(&self, domain: &str, port: u16) -> bool {
        use crate::protocol::EgressMode;

        let allowed = match self.policy.mode {
            EgressMode::Deny => false,
            EgressMode::Open => true,
            EgressMode::Allowlist => self
                .policy
                .domains
                .iter()
                .any(|pattern| matches_domain(pattern, domain)),
        };

        self.record(domain, port, allowed).await;
        allowed
    }

    /// Return a snapshot of the egress log.
    pub async fn log_entries(&self) -> Vec<LogEntry> {
        self.log.read().await.iter().cloned().collect()
    }

    /// Update the proxy policy at runtime (e.g. after a hot-patch).
    pub async fn set_policy(&mut self, policy: EgressPolicy) {
        self.policy = policy;
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn record(&self, domain: &str, port: u16, allowed: bool) {
        let entry = LogEntry {
            sandbox_id: self.sandbox_id.clone(),
            domain: domain.to_string(),
            port,
            allowed,
            timestamp: SystemTime::now(),
        };

        let mut log = self.log.write().await;
        if log.len() >= self.log_capacity {
            log.pop_front();
        }
        log.push_back(entry);
    }
}

/// Match a domain against a pattern that may contain a leading `*` wildcard.
///
/// Examples:
/// - `"example.com"` matches only `"example.com"`.
/// - `"*.example.com"` matches `"api.example.com"` but not `"example.com"`.
pub fn matches_domain(pattern: &str, domain: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // Wildcard: domain must have at least one label before the suffix.
        domain.ends_with(suffix)
            && domain.len() > suffix.len() + 1
            && domain.as_bytes()[domain.len() - suffix.len() - 1] == b'.'
    } else {
        // Exact match (case-insensitive).
        pattern.eq_ignore_ascii_case(domain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_domain_match() {
        assert!(matches_domain("example.com", "example.com"));
        assert!(!matches_domain("example.com", "other.com"));
    }

    #[test]
    fn wildcard_subdomain_match() {
        assert!(matches_domain("*.example.com", "api.example.com"));
        assert!(matches_domain("*.example.com", "a.b.example.com"));
        assert!(!matches_domain("*.example.com", "example.com"));
        assert!(!matches_domain("*.example.com", "notexample.com"));
    }

    #[tokio::test]
    async fn deny_policy_blocks_all() {
        let proxy = EgressProxy::new(
            "test".into(),
            EgressPolicy {
                mode: crate::protocol::EgressMode::Deny,
                domains: vec![],
            },
        );
        assert!(!proxy.check("example.com", 443).await);
    }

    #[tokio::test]
    async fn open_policy_allows_all() {
        let proxy = EgressProxy::new(
            "test".into(),
            EgressPolicy {
                mode: crate::protocol::EgressMode::Open,
                domains: vec![],
            },
        );
        assert!(proxy.check("example.com", 443).await);
    }

    #[tokio::test]
    async fn allowlist_policy() {
        let proxy = EgressProxy::new(
            "test".into(),
            EgressPolicy {
                mode: crate::protocol::EgressMode::Allowlist,
                domains: vec!["api.example.com".into(), "*.cdn.net".into()],
            },
        );
        assert!(proxy.check("api.example.com", 443).await);
        assert!(proxy.check("static.cdn.net", 443).await);
        assert!(!proxy.check("evil.com", 80).await);
    }
}
