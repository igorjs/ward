// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Per-sandbox egress proxy with domain allowlist enforcement.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

use crate::protocol::EgressPolicy;

/// Cap on the CONNECT request header we'll read before giving up, and on the
/// number of header lines, to bound work from a hostile client.
const MAX_HEADER_LINES: usize = 100;

/// Timeout for resolving a CONNECT target host via DNS. Lookups normally
/// return in well under a second; this bounds a stalled or unresponsive
/// resolver so one bad hostname can't tie up a proxy task indefinitely.
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for a single upstream TCP connect attempt. A reachable target
/// completes its handshake in milliseconds; this bounds an attempt against
/// a target whose handshake never completes (e.g. a saturated or
/// black-holed listener) so the proxy can fail over to the next resolved
/// address, or give up, promptly instead of blocking the task forever.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle timeout for an established egress tunnel. A flat timeout on the
/// whole transfer would kill a long but legitimate download, so this
/// bounds inactivity instead: the deadline resets whenever either
/// direction reads a byte, and the tunnel is only torn down once BOTH
/// directions have been silent for the full window.
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

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

        // Metrics: labelled by decision so operators can alert on the
        // ratio (a sudden spike in `denied` may indicate a runaway
        // sandbox or a misconfigured allowlist).
        let decision = if allowed { "allowed" } else { "denied" };
        metrics::counter!("wardd_egress_check_total", "decision" => decision).increment(1);

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

// ---------------------------------------------------------------------------
// Forward-proxy server
// ---------------------------------------------------------------------------

impl EgressProxy {
    /// Serve HTTP `CONNECT` forward-proxy requests on `listener` until it
    /// stops yielding connections. Each request is checked against the
    /// policy: allowed targets are tunnelled byte-for-byte, denied ones get
    /// a `403`. Routing a sandbox's traffic into this listener is the
    /// (Linux-gated) TAP step; the proxy itself is transport-agnostic.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((client, _peer)) => {
                    let proxy = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = proxy.handle_connect(client).await {
                            tracing::debug!(error = %e, "egress connection ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "egress proxy accept failed");
                    break;
                }
            }
        }
    }

    async fn handle_connect(self: Arc<Self>, mut client: TcpStream) -> std::io::Result<()> {
        let Some((host, port)) = read_connect_target(&mut client).await? else {
            client
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await?;
            return Ok(());
        };
        // SEC-023: strip IPv6 brackets so tokio's connect resolves cleanly
        // and the policy matcher sees the same host string the resolver does.
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();

        if !self.check(&host, port).await {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await?;
            return Ok(());
        }

        // SEC-005: resolve the target server-side and refuse if ANY resolved
        // address falls in a private / loopback / link-local / multicast /
        // unique-local range. Without this guard, a sandbox in Open egress
        // mode (or Allowlist with an IP literal) can CONNECT
        // 169.254.169.254:80 and hit the cloud metadata service from inside
        // the security boundary — a confused-deputy SSRF.
        //
        // DNS-rebinding defence: we collect SocketAddrs from the resolve
        // and connect to them DIRECTLY (never re-resolving the host
        // string). A hostile resolver that returns a public IP first and
        // a private IP second would otherwise bypass the check between
        // resolve and connect.
        let Some(resolved) = resolve_target(
            tokio::net::lookup_host((host.as_str(), port)),
            DNS_LOOKUP_TIMEOUT,
        )
        .await
        else {
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?;
            return Ok(());
        };
        if resolved.iter().any(|sa| is_private_or_local(&sa.ip())) {
            tracing::warn!(
                sandbox = %self.sandbox_id,
                host = %host,
                "egress: rejected target resolving to private/local IP"
            );
            client.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await?;
            return Ok(());
        }

        // Try each resolved SocketAddr in order. Connecting to a concrete
        // address skips the second DNS lookup tokio does for (host, port)
        // tuples, closing the rebinding window.
        let Some(mut upstream) = connect_first_reachable(&resolved, UPSTREAM_CONNECT_TIMEOUT).await
        else {
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                .await?;
            return Ok(());
        };

        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        copy_bidirectional_with_idle_timeout(&mut client, &mut upstream, TUNNEL_IDLE_TIMEOUT)
            .await?;
        Ok(())
    }
}

/// Resolve a CONNECT target, bounded by `timeout` so a stalled or
/// unresponsive resolver can't tie up a proxy task indefinitely. Accepts
/// the lookup as a future (rather than performing it directly) so tests can
/// substitute one that never resolves without needing control over the
/// host's actual DNS resolver. Returns `None` on either a resolution
/// failure or a timeout; callers can't distinguish the two today because a
/// CONNECT client gets the same 502 response either way.
async fn resolve_target<F, I>(lookup: F, timeout: Duration) -> Option<Vec<std::net::SocketAddr>>
where
    F: std::future::Future<Output = std::io::Result<I>>,
    I: Iterator<Item = std::net::SocketAddr>,
{
    match tokio::time::timeout(timeout, lookup).await {
        Ok(Ok(addrs)) => Some(addrs.collect()),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Attempt each of `addrs` in order, giving each connect up to `timeout`
/// before moving to the next. Returns the first successful connection, or
/// `None` if every address failed or timed out.
async fn connect_first_reachable(
    addrs: &[std::net::SocketAddr],
    timeout: Duration,
) -> Option<TcpStream> {
    for sa in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(timeout, TcpStream::connect(sa)).await {
            return Some(s);
        }
    }
    None
}

/// Pump bytes bidirectionally between `client` and `upstream` until either
/// side closes, tearing the tunnel down once BOTH directions have gone
/// quiet for `idle_timeout`. See `TUNNEL_IDLE_TIMEOUT` for why this bounds
/// inactivity rather than the whole transfer's duration.
async fn copy_bidirectional_with_idle_timeout(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    idle_timeout: Duration,
) -> std::io::Result<()> {
    let last_activity = std::sync::Mutex::new(tokio::time::Instant::now());
    let (mut client_r, mut client_w) = client.split();
    let (mut upstream_r, mut upstream_w) = upstream.split();

    tokio::try_join!(
        pump(&mut client_r, &mut upstream_w, idle_timeout, &last_activity),
        pump(&mut upstream_r, &mut client_w, idle_timeout, &last_activity),
    )?;
    Ok(())
}

/// Copy from `src` to `dst` until EOF, failing with `ErrorKind::TimedOut`
/// if `idle_timeout` elapses since the last byte read on EITHER direction.
/// `last_activity` is shared with the opposite-direction pump so a busy
/// download doesn't get killed just because the client isn't sending
/// anything back upstream: each time this pump's own wait lapses, it
/// rechecks the shared deadline before giving up, in case the other
/// direction refreshed it in the meantime.
async fn pump<R, W>(
    src: &mut R,
    dst: &mut W,
    idle_timeout: Duration,
    last_activity: &std::sync::Mutex<tokio::time::Instant>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        let deadline = *last_activity.lock().unwrap() + idle_timeout;
        match tokio::time::timeout_at(deadline, src.read(&mut buf)).await {
            Err(_) => {
                if *last_activity.lock().unwrap() + idle_timeout > tokio::time::Instant::now() {
                    continue; // the other direction refreshed activity; keep waiting
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "egress tunnel idle timeout",
                ));
            }
            Ok(Ok(0)) => {
                let _ = dst.shutdown().await;
                return Ok(());
            }
            Ok(Ok(n)) => {
                *last_activity.lock().unwrap() = tokio::time::Instant::now();
                dst.write_all(&buf[..n]).await?;
            }
            Ok(Err(e)) => return Err(e),
        }
    }
}

/// Read a `CONNECT host:port HTTP/1.1` request line plus its headers from
/// `client`, returning the parsed target. Returns `Ok(None)` for anything
/// that isn't a well-formed CONNECT. A `CONNECT` client waits for the proxy's
/// response before sending tunnel bytes, so buffering the header here does
/// not swallow payload.
async fn read_connect_target(client: &mut TcpStream) -> std::io::Result<Option<(String, u16)>> {
    let mut reader = tokio::io::BufReader::new(client);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(None);
    }

    // Drain remaining headers up to the blank line (bounded).
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Ok(None);
    }

    // target is host:port; rsplit so IPv6 literals keep their inner colons.
    let Some((host, port_str)) = target.rsplit_once(':') else {
        return Ok(None);
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return Ok(None);
    };
    if host.is_empty() {
        return Ok(None);
    }
    Ok(Some((host.to_string(), port)))
}

/// SEC-005: return true if `ip` is in any range that should not be
/// reachable via the egress proxy: private (RFC1918), loopback,
/// link-local (catches 169.254.169.254 cloud metadata), multicast,
/// unspecified, broadcast, CGNAT, IETF/TEST-NET/benchmarking/future-use
/// reserved (IPv4), and unique-local / 6to4 / NAT64 / Teredo (IPv6).
/// IPv4-mapped IPv6 addresses recurse so they inherit the full IPv4
/// rule set.
fn is_private_or_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let oct = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 0.0.0.0/8 reserved (Linux routes to loopback in some setups)
                || oct[0] == 0
                // 100.64.0.0/10 CGNAT
                || (oct[0] == 100 && (oct[1] & 0xC0) == 0x40)
                // 192.0.0.0/24 IETF protocol assignments (incl. 192.0.0.1)
                || (oct[0] == 192 && oct[1] == 0 && oct[2] == 0)
                // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 TEST-NET
                || (oct[0] == 192 && oct[1] == 0 && oct[2] == 2)
                || (oct[0] == 198 && oct[1] == 51 && oct[2] == 100)
                || (oct[0] == 203 && oct[1] == 0 && oct[2] == 113)
                // 198.18.0.0/15 benchmarking
                || (oct[0] == 198 && (oct[1] & 0xFE) == 18)
                // 240.0.0.0/4 future-use (often unfiltered, reaches host)
                || oct[0] >= 240
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fe80::/10 link-local
                || (seg[0] & 0xFFC0 == 0xFE80)
                // fc00::/7 unique local
                || (seg[0] & 0xFE00 == 0xFC00)
                // 2002::/16 6to4 anycast — tunnels to arbitrary IPv4
                || seg[0] == 0x2002
                // 64:ff9b::/96 NAT64 — also tunnels to IPv4
                || (seg[0] == 0x0064 && seg[1] == 0xFF9B)
                // 2001::/32 Teredo tunnels
                || (seg[0] == 0x2001 && seg[1] == 0x0000)
                // IPv4-mapped IPv6 — recurse so the full v4 rule set applies.
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| is_private_or_local(&std::net::IpAddr::V4(v4)))
                    .unwrap_or(false)
        }
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

    // ----- forward-proxy server -------------------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Start an echo server on loopback; returns its port. Each connection
    /// echoes back whatever it receives until EOF.
    async fn spawn_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// Start the proxy server on loopback; returns its port.
    async fn spawn_proxy(proxy: Arc<EgressProxy>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(proxy.serve(listener));
        port
    }

    /// Read a full HTTP response header (through the terminating blank line)
    /// so no header bytes are left to corrupt subsequent tunnel reads.
    async fn read_response_head(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// Regression for SEC-005: even with `EgressMode::Open` the proxy MUST
    /// refuse targets that resolve to a private / loopback / link-local
    /// address. Pre-SEC-005 this test asserted the opposite (Open mode
    /// tunnels bytes through to loopback); after the patch, the SSRF
    /// guard rejects the loopback target server-side and returns 403.
    /// Tunnel-bytes behaviour for genuine public targets is not unit-
    /// testable without network access; the `check()` arms are covered
    /// by `given_open_policy_when_check_any_domain_then_returns_true`.
    #[tokio::test]
    async fn given_open_policy_when_connect_to_loopback_then_refused_by_ssrf_guard() {
        // Arrange: an echo upstream we'd LIKE to reach if the guard
        // weren't in the way, and an Open proxy in front of it.
        let echo_port = spawn_echo_server().await;
        let proxy = Arc::new(build_proxy(EgressMode::Open, vec![]));
        let proxy_port = spawn_proxy(Arc::clone(&proxy)).await;

        // Act
        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let status = read_response_head(&mut client).await;

        // Assert: SEC-005 guard fires after policy check but before
        // upstream connect. The log entry still shows allowed=true
        // because check() ran first (the guard is a separate layer);
        // operators inspecting the audit trail need both signals to
        // understand why a connect didn't reach its target.
        assert!(
            status.contains("403"),
            "expected 403 (SEC-005 loopback guard), got {status:?}"
        );
        let log = proxy.log_entries().await;
        assert!(
            log.iter().any(|e| e.domain == "127.0.0.1" && e.allowed),
            "Open-policy check() should still log the attempt as policy-allowed"
        );
    }

    #[tokio::test]
    async fn given_deny_policy_when_connect_then_returns_403() {
        // Arrange
        let proxy = Arc::new(build_proxy(EgressMode::Deny, vec![]));
        let proxy_port = spawn_proxy(Arc::clone(&proxy)).await;

        // Act
        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        client
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let status = read_response_head(&mut client).await;

        // Assert: rejected, and the attempt is logged as denied.
        assert!(status.contains("403"), "expected 403, got {status:?}");
        let log = proxy.log_entries().await;
        assert!(log.iter().any(|e| e.domain == "example.com" && !e.allowed));
    }

    #[tokio::test]
    async fn given_allowlist_when_connect_to_unlisted_then_returns_403() {
        // Arrange: allow only example.com; connect to the echo server (which
        // presents as 127.0.0.1), which is not listed.
        let echo_port = spawn_echo_server().await;
        let proxy = Arc::new(build_proxy(EgressMode::Allowlist, vec!["example.com"]));
        let proxy_port = spawn_proxy(Arc::clone(&proxy)).await;

        // Act
        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let status = read_response_head(&mut client).await;

        // Assert
        assert!(status.contains("403"), "expected 403, got {status:?}");
    }

    // ----- timeout enforcement (resolve / connect / idle tunnel) ----------
    //
    // Coverage targets:
    //   - resolve_target: happy path and a resolver that never returns
    //   - connect_first_reachable: happy path and a target whose connect
    //     backlog is saturated so the handshake never completes
    //   - copy_bidirectional_with_idle_timeout: happy path and both
    //     directions silent past the idle window
    //
    // A real DNS resolver can't be made to hang deterministically in a
    // test (it isn't under our control), so resolve_target's timeout test
    // substitutes a future that never resolves. connect_first_reachable
    // and the idle-tunnel pump use genuine loopback sockets, matching the
    // rest of this module's test style. All timeout assertions use
    // `tokio::time::pause`/`advance` so no test waits on real wall time.

    /// Bind a listener and open connections to it, without ever accepting,
    /// until one genuinely blocks: the kernel's connect backlog is full.
    /// Returns the listener and the prior connections (both kept alive so
    /// the backlog stays saturated) plus the address a further connect
    /// will hang against.
    async fn saturate_backlog() -> (TcpListener, std::net::SocketAddr, Vec<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut conns = Vec::new();
        // Stops as soon as an attempt doesn't complete in time: that's the
        // signal the backlog is full.
        while let Ok(Ok(s)) =
            tokio::time::timeout(Duration::from_millis(200), TcpStream::connect(addr)).await
        {
            conns.push(s);
        }
        (listener, addr, conns)
    }

    /// Bind a loopback listener, connect to it, and accept that connection,
    /// returning both ends. Stands in for the two legs (client-facing and
    /// upstream-facing) a real tunnel pumps between.
    async fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (a, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (a.unwrap(), accepted.unwrap().0)
    }

    #[tokio::test]
    async fn given_lookup_resolves_when_resolve_target_then_returns_addrs() {
        // Arrange
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 443));
        let lookup = std::future::ready(Ok::<_, std::io::Error>(vec![addr].into_iter()));

        // Act
        let result = resolve_target(lookup, Duration::from_secs(5)).await;

        // Assert
        assert_eq!(result, Some(vec![addr]));
    }

    #[tokio::test]
    async fn given_lookup_never_resolves_when_resolve_target_then_times_out_and_returns_none() {
        // Arrange: a resolver future that never completes, standing in for
        // a stalled DNS server.
        let never_resolves =
            std::future::pending::<std::io::Result<std::vec::IntoIter<std::net::SocketAddr>>>();
        let timeout = Duration::from_millis(50);
        tokio::time::pause();

        // Act
        let fut = resolve_target(never_resolves, timeout);
        tokio::pin!(fut);
        tokio::time::advance(timeout + Duration::from_millis(1)).await;
        let result = fut.await;

        // Assert
        assert!(result.is_none(), "expected a stalled resolver to time out");
    }

    #[tokio::test]
    async fn given_reachable_target_when_connect_first_reachable_then_returns_connection() {
        // Arrange
        let echo_port = spawn_echo_server().await;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], echo_port));

        // Act
        let result = connect_first_reachable(&[addr], Duration::from_secs(5)).await;

        // Assert
        assert!(result.is_some(), "expected a live target to connect");
    }

    #[tokio::test]
    async fn given_target_backlog_saturated_when_connect_first_reachable_then_times_out_and_returns_none()
     {
        // Arrange: fill the target's connect backlog so its handshake
        // never completes.
        let (listener, addr, _conns) = saturate_backlog().await;
        let addrs = [addr];
        tokio::time::pause();

        // Act
        let fut = connect_first_reachable(&addrs, UPSTREAM_CONNECT_TIMEOUT);
        tokio::pin!(fut);
        tokio::time::advance(UPSTREAM_CONNECT_TIMEOUT + Duration::from_millis(1)).await;
        let result = fut.await;

        // Assert
        assert!(
            result.is_none(),
            "expected a hung handshake to time out and be treated as unreachable"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn given_active_traffic_when_within_idle_timeout_then_bytes_are_copied_end_to_end() {
        // Arrange
        let (mut client_side, mut client_peer) = connected_pair().await;
        let (mut upstream_side, mut upstream_peer) = connected_pair().await;
        let idle_timeout = Duration::from_secs(5);

        // Act: run the pump concurrently while driving traffic through it.
        let pump_task = tokio::spawn(async move {
            copy_bidirectional_with_idle_timeout(&mut client_side, &mut upstream_side, idle_timeout)
                .await
        });

        client_peer.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        upstream_peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        upstream_peer.write_all(b"pong!").await.unwrap();
        let mut buf2 = [0u8; 5];
        client_peer.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"pong!");

        // Closing both peers surfaces EOF on both directions so the pump
        // finishes cleanly instead of running for the life of the test.
        drop(client_peer);
        drop(upstream_peer);
        let result = pump_task.await.unwrap();

        // Assert
        assert!(result.is_ok(), "expected a clean shutdown, got {result:?}");
    }

    #[tokio::test]
    async fn given_both_directions_silent_when_idle_timeout_elapses_then_returns_timed_out_error() {
        // Arrange: two connected pairs with peers kept open but silent, so
        // neither direction ever produces a byte.
        let (mut client_side, _client_peer) = connected_pair().await;
        let (mut upstream_side, _upstream_peer) = connected_pair().await;
        let idle_timeout = Duration::from_millis(50);
        tokio::time::pause();

        // Act
        let fut = copy_bidirectional_with_idle_timeout(
            &mut client_side,
            &mut upstream_side,
            idle_timeout,
        );
        tokio::pin!(fut);
        tokio::time::advance(idle_timeout + Duration::from_millis(1)).await;
        let result = fut.await;

        // Assert
        let err = result.expect_err("expected the idle tunnel to time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
