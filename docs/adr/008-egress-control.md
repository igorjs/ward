# ADR-008: Egress Control and Network Isolation

**Status:** Accepted (forward proxy implemented; in-VM traffic routing gated)
**Date:** 2026-05-02
**Authors:** Igor

## Context

A sandbox without network controls is not a sandbox. If arbitrary code can reach the internet, it can exfiltrate data, download malware, call home to a C2 server, or abuse credentials found in the environment. Docker provides no egress filtering by default.

Egress control is one of Ward's key differentiators. It must be simple to configure (a list of allowed domains) and enforced at the network level (not bypassable by the sandboxed process).

## Decision

### Default: deny all egress

By default, a Ward sandbox has no outbound network access. The sandboxed process cannot reach the internet, the local network, or any other host.

### Allowlist mode

Users can specify a list of allowed domains in the sandbox configuration. In protobuf:

```protobuf
EgressPolicy {
  EgressMode mode = 1;       // DENY (default) | ALLOWLIST | OPEN
  repeated string domains = 2;
}
```

Wildcard prefixes (`*.example.com`) are supported for subdomains. Bare wildcards (`*`) are not allowed as they would negate the purpose of the allowlist.

### Enforcement mechanism

libkrun's microVM gives each sandbox its own kernel and network stack. Ward's `EgressProxy` (in `ward-core/src/egress/proxy.rs`) implements an embedded forward proxy that the VM's outbound traffic is routed through:

- HTTP CONNECT tunnelling for HTTPS traffic
- Plain HTTP forwarding
- Domain validation against the allowlist
- Connection logging (which domain, when, allowed/denied)
- Timeout enforcement per connection

DNS resolution happens on the host side, preventing DNS-based bypasses. The proxy only allows connections to IPs that resolved from allowed domains, blocking direct IP bypasses.

### Current implementation state

The `EgressProxy` implements a working HTTP CONNECT forward proxy: it parses the CONNECT target, evaluates it against the policy (deny/open/allowlist), logs the decision, and either tunnels the connection or returns `403`. This is covered by tests over loopback, and `GetEgressLog` is wired. **What remains is routing each sandbox's traffic into its proxy** — attaching a TAP device (Linux) and redirecting guest egress through it — which is gated behind the `krunvm` boot path. The CLI / API surface is stable.

### What the proxy handles

- HTTP CONNECT tunnelling for HTTPS traffic
- Plain HTTP forwarding
- Domain validation against the allowlist
- Connection logging
- Timeout enforcement per connection

### What the proxy does not handle

- Deep packet inspection (not a goal)
- Content filtering (not a goal)
- Bandwidth throttling (resource limits handle this at the VM level)
- Ingress (sandboxes are not reachable from outside)

### Open mode

For use cases where egress filtering is not needed (trusted code, internal tooling):

```protobuf
EgressPolicy { mode: EGRESS_MODE_OPEN }
```

Ward logs a warning when a sandbox is created with open egress.

### Logging

All egress attempts (allowed and denied) are logged by the daemon and available via the `GetEgressLog` RPC. This provides an audit trail of every outbound connection a sandboxed process attempted.

## Consequences

- Default-deny means sandboxes are safe out of the box. Users must explicitly opt in to network access.
- The forward proxy adds latency to outbound connections (one extra hop). For the typical use case (npm install, pip install, git clone), this is negligible.
- The proxy is embedded in the daemon, not a separate process.
- Domain-level filtering (not IP-level) means the proxy must resolve DNS and maintain a mapping. This prevents the common bypass where a sandboxed process resolves a domain to an IP and then connects directly to the IP.
- The egress log provides visibility into what a sandboxed agent or CI job actually accessed.
