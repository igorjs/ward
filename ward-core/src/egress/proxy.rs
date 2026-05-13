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

// ---------------------------------------------------------------------------
// Tests
//
// Style: each test follows BDD naming (`given_X_when_Y_then_Z`) with AAA
// (Arrange / Act / Assert) phases marked in the body.
//
// Coverage targets:
//   - matches_domain: every branch of the matcher (exact / wildcard / case)
//   - EgressProxy::check: each EgressMode arm, both allow and deny outcomes
//   - record(): log retention rolls over correctly at log_capacity
//   - log_entries(): snapshot ordering and contents
//   - set_policy(): runtime policy updates take effect on subsequent checks
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::EgressMode;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    // ----- Helpers --------------------------------------------------------

    /// Build an EgressProxy for tests with a known sandbox ID and policy.
    fn build_proxy(mode: EgressMode, domains: Vec<&str>) -> EgressProxy {
        EgressProxy::new(
            "test-sandbox".into(),
            EgressPolicy {
                mode,
                domains: domains.into_iter().map(String::from).collect(),
            },
        )
    }

    // ----- matches_domain: exact-match branch -----------------------------

    #[rstest]
    #[case::same("example.com", "example.com")]
    #[case::case_insensitive_pattern("EXAMPLE.com", "example.com")]
    #[case::case_insensitive_domain("example.com", "Example.COM")]
    fn given_exact_pattern_when_domain_equals_then_matches(
        #[case] pattern: &str,
        #[case] domain: &str,
    ) {
        // Arrange: (pattern, domain) from #[case]

        // Act
        let result = matches_domain(pattern, domain);

        // Assert
        assert!(result, "{pattern:?} should match {domain:?}");
    }

    #[rstest]
    #[case::different("example.com", "other.com")]
    #[case::longer("example.com", "example.com.evil.com")]
    #[case::shorter("example.com", "ample.com")]
    #[case::substring("example.com", "example.co")]
    fn given_exact_pattern_when_domain_differs_then_does_not_match(
        #[case] pattern: &str,
        #[case] domain: &str,
    ) {
        // Act
        let result = matches_domain(pattern, domain);

        // Assert
        assert!(!result, "{pattern:?} must not match {domain:?}");
    }

    // ----- matches_domain: wildcard branch --------------------------------

    #[rstest]
    #[case::one_label("*.example.com", "api.example.com")]
    #[case::two_labels("*.example.com", "a.b.example.com")]
    #[case::deep_nesting("*.example.com", "x.y.z.example.com")]
    fn given_wildcard_pattern_when_subdomain_matches_then_returns_true(
        #[case] pattern: &str,
        #[case] domain: &str,
    ) {
        // Act
        let result = matches_domain(pattern, domain);

        // Assert
        assert!(result, "{pattern:?} should match {domain:?}");
    }

    #[rstest]
    #[case::apex("*.example.com", "example.com")]
    #[case::different_root("*.example.com", "example.org")]
    #[case::sneaky_substring("*.example.com", "notexample.com")]
    #[case::partial_label("*.example.com", "evilexample.com")]
    fn given_wildcard_pattern_when_domain_not_subdomain_then_returns_false(
        #[case] pattern: &str,
        #[case] domain: &str,
    ) {
        // Act
        let result = matches_domain(pattern, domain);

        // Assert
        assert!(
            !result,
            "{pattern:?} must NOT match {domain:?} (apex / sibling root)",
        );
    }

    #[test]
    fn given_wildcard_pattern_when_domain_is_just_the_suffix_with_dot_then_does_not_match() {
        // Arrange: regression guard — ".example.com" used to slip through
        // older implementations that only checked ends_with.

        // Act
        let result = matches_domain("*.example.com", ".example.com");

        // Assert
        assert!(
            !result,
            "a bare dot-prefixed suffix must not satisfy the wildcard"
        );
    }

    // ----- EgressProxy::check by policy mode ------------------------------

    #[tokio::test]
    async fn given_deny_policy_when_check_any_domain_then_returns_false() {
        // Arrange
        let proxy = build_proxy(EgressMode::Deny, vec![]);

        // Act
        let allowed = proxy.check("example.com", 443).await;

        // Assert
        assert!(!allowed, "Deny policy must reject every domain");
    }

    #[tokio::test]
    async fn given_open_policy_when_check_any_domain_then_returns_true() {
        // Arrange
        let proxy = build_proxy(EgressMode::Open, vec![]);

        // Act
        let allowed = proxy.check("anything.example", 1234).await;

        // Assert
        assert!(allowed, "Open policy must allow every domain");
    }

    #[tokio::test]
    async fn given_allowlist_policy_when_domain_matches_then_returns_true() {
        // Arrange
        let proxy = build_proxy(EgressMode::Allowlist, vec!["api.example.com"]);

        // Act
        let allowed = proxy.check("api.example.com", 443).await;

        // Assert
        assert!(allowed);
    }

    #[tokio::test]
    async fn given_allowlist_policy_when_domain_not_listed_then_returns_false() {
        // Arrange
        let proxy = build_proxy(EgressMode::Allowlist, vec!["api.example.com"]);

        // Act
        let allowed = proxy.check("evil.com", 443).await;

        // Assert
        assert!(!allowed);
    }

    #[tokio::test]
    async fn given_allowlist_with_wildcard_when_subdomain_matches_then_returns_true() {
        // Arrange
        let proxy = build_proxy(EgressMode::Allowlist, vec!["*.cdn.net"]);

        // Act
        let allowed = proxy.check("static.cdn.net", 443).await;

        // Assert
        assert!(allowed);
    }

    #[tokio::test]
    async fn given_empty_allowlist_when_check_any_domain_then_returns_false() {
        // Arrange: empty allowlist == deny everything, but distinct from Deny mode
        let proxy = build_proxy(EgressMode::Allowlist, vec![]);

        // Act
        let allowed = proxy.check("example.com", 443).await;

        // Assert
        assert!(!allowed, "an empty allowlist must reject every domain");
    }

    // ----- Log recording --------------------------------------------------

    #[tokio::test]
    async fn given_check_when_domain_allowed_then_log_entry_is_recorded() {
        // Arrange
        let proxy = build_proxy(EgressMode::Open, vec![]);

        // Act
        let _ = proxy.check("example.com", 443).await;

        // Assert
        let log = proxy.log_entries().await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].sandbox_id, "test-sandbox");
        assert_eq!(log[0].domain, "example.com");
        assert_eq!(log[0].port, 443);
        assert!(log[0].allowed);
    }

    #[tokio::test]
    async fn given_check_when_domain_denied_then_log_entry_still_recorded() {
        // Arrange: denied connections are still logged — that's the audit trail
        let proxy = build_proxy(EgressMode::Deny, vec![]);

        // Act
        let _ = proxy.check("example.com", 443).await;

        // Assert
        let log = proxy.log_entries().await;
        assert_eq!(log.len(), 1);
        assert!(
            !log[0].allowed,
            "denial must be recorded with allowed=false"
        );
    }

    #[tokio::test]
    async fn given_multiple_checks_when_log_read_then_entries_in_chronological_order() {
        // Arrange
        let proxy = build_proxy(EgressMode::Allowlist, vec!["a.com"]);

        // Act
        let _ = proxy.check("a.com", 80).await;
        let _ = proxy.check("b.com", 443).await;
        let _ = proxy.check("c.com", 8080).await;

        // Assert
        let log = proxy.log_entries().await;
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].domain, "a.com");
        assert_eq!(log[1].domain, "b.com");
        assert_eq!(log[2].domain, "c.com");
    }

    #[tokio::test]
    async fn given_log_at_capacity_when_new_entry_then_oldest_is_evicted() {
        // Arrange: build a proxy with a tiny capacity so we can verify
        // ring-buffer behaviour without writing 1000 entries.
        let mut proxy = build_proxy(EgressMode::Open, vec![]);
        proxy.log_capacity = 3;

        // Act: write 4 entries into a 3-slot buffer.
        let _ = proxy.check("one.com", 80).await;
        let _ = proxy.check("two.com", 80).await;
        let _ = proxy.check("three.com", 80).await;
        let _ = proxy.check("four.com", 80).await;

        // Assert: oldest entry was dropped; newest is at the tail.
        let log = proxy.log_entries().await;
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].domain, "two.com", "one.com should have been evicted");
        assert_eq!(log[2].domain, "four.com");
    }

    // ----- set_policy: runtime updates ------------------------------------

    #[tokio::test]
    async fn given_existing_proxy_when_policy_updated_to_open_then_subsequent_check_allows() {
        // Arrange: start in Deny mode
        let mut proxy = build_proxy(EgressMode::Deny, vec![]);
        assert!(!proxy.check("example.com", 443).await, "precondition");

        // Act: flip policy at runtime
        proxy
            .set_policy(EgressPolicy {
                mode: EgressMode::Open,
                domains: vec![],
            })
            .await;
        let allowed_after = proxy.check("example.com", 443).await;

        // Assert
        assert!(allowed_after, "policy update must affect subsequent checks");
    }

    #[tokio::test]
    async fn given_existing_proxy_when_policy_updated_then_old_log_preserved() {
        // Arrange: log a denied attempt under the original policy
        let mut proxy = build_proxy(EgressMode::Deny, vec![]);
        let _ = proxy.check("blocked.com", 443).await;

        // Act: change policy and log another attempt
        proxy
            .set_policy(EgressPolicy {
                mode: EgressMode::Open,
                domains: vec![],
            })
            .await;
        let _ = proxy.check("allowed.com", 443).await;

        // Assert: both entries are present; policy change does NOT clear history
        let log = proxy.log_entries().await;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].domain, "blocked.com");
        assert!(!log[0].allowed);
        assert_eq!(log[1].domain, "allowed.com");
        assert!(log[1].allowed);
    }
}
