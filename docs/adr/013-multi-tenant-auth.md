# ADR-013: Multi-Tenant Authentication and Authorisation

**Status:** Proposed (no implementation yet)
**Date:** 2026-05-27
**Authors:** Igor

## Context

ward today is a **single-user daemon**:

- `wardd` binds a Unix domain socket at mode `0600` under `$HOME/.ward` (or
  `$XDG_RUNTIME_DIR/ward/`) — only the invoking OS user can connect.
- The gRPC API has no authn layer: any client connecting to the socket
  can call any method (the OS-level peer-UID check via the socket is the
  authentication boundary).
- The gRPC API has no authz layer: every connected caller has every
  permission. Per-sandbox ownership exists in `SandboxManager` but
  ultimately every authenticated client could `remove_sandbox` any
  sandbox.

This is appropriate for the documented usage today: a developer running
`wardd` for their own sandboxes. It is **not** appropriate for:

1. **Shared developer host.** Multiple OS users want to use one `wardd`
   instance (e.g. a build server, a dev VM). Today this requires running
   `wardd` per user; impractical at scale.
2. **On-prem multi-user daemon.** Internal sandbox service exposed to
   teammates / CI runners / a control plane. The implicit-peer-UID model
   doesn't extend.
3. **Remote access.** [ADR-004](004-ipc-protocol.md) gestures at "Remote
   (TCP): API key in gRPC metadata (`authorization: Bearer ward-key-xxx`).
   mTLS for daemon-to-daemon." Neither is implemented. The audit
   ([analysis/main/raw-audit.md](../../analysis/main/raw-audit.md))
   flagged the gap (SEC-008 cluster).

This ADR proposes the shape of multi-tenant authn/authz when (if) the
use case arrives, so future-Igor isn't designing it from scratch under
deadline pressure.

## Decision

**Defer implementation. Document the shape.**

For v0.x, ward stays single-user-per-daemon. The `wardd` invocation
remains "one process, one OS user, one Unix socket". Operators who need
multi-user today run one `wardd` per OS user.

When a concrete multi-tenant use case lands, implement the following:

### Authentication

Three transport modes, two auth schemes:

| Mode | Transport | Authn |
|---|---|---|
| Local single-user | Unix socket 0600 | Peer-UID (current behaviour, unchanged) |
| Local multi-user | Unix socket 0660 + group | Peer-UID + group ACL (operating-system-level) |
| Remote | TCP + TLS | mTLS client cert OR `Authorization: Bearer <jwt>` |

JWT signing key configured via `WARD_AUTH_PUBLIC_KEY_PATH`. mTLS roots
via `WARD_TLS_CA_PATH`. Both off by default; setting either enables that
transport.

A tonic `Interceptor` extracts the principal from the request:
- Unix peer-uid → `principal::Local { uid: u32 }`
- mTLS cert → `principal::Cert { subject: String, fingerprint: [u8; 32] }`
- JWT → `principal::Token { sub: String, iss: String, exp: u64, scopes: Vec<String> }`

Stored on the request extensions for the handler layer to use.

### Authorisation

A `Permission` enum tied to each gRPC method:

```rust
enum Permission {
    SandboxCreate,
    SandboxList,
    SandboxRead { id: String },
    SandboxRemove { id: String },
    Exec { sandbox_id: String },
    Snapshot { sandbox_id: String },
    Publish { topic: String },
    Subscribe { topic: String },
    // ...
}
```

A `Policy` trait (with a default impl) decides `is_authorised(principal,
permission) -> bool`. Out of the box: principals own the sandboxes they
created and can do anything to them; can't see anyone else's. Override
via config file for richer rules (admin user, read-only viewers, etc.).

### Sandbox ownership

Already partially exists via `SandboxManager`'s per-sandbox tracking;
extend by storing the creating principal on each `SandboxEntry`. List
operations filter to caller-owned by default; admin principals see all.

## Why defer

- **No live use case today.** Designing speculatively risks the wrong
  abstractions. Better to wait for "user X needs to run wardd at $WORK
  for the team" and design against that need.
- **mTLS + JWT + RBAC adds significant attack surface.** Every layer is
  a place to mis-configure. Doing it well needs incident-driven iteration,
  not greenfield design.
- **The peer-UID model is genuinely correct for today's users.** Anyone
  who can connect to `0600 ~/.ward/ward.sock` is already the daemon's
  owner; nothing to authenticate further.
- **Cargo-cult RBAC for OSS sandbox tools is common and wrong.** This
  ADR captures the intent so we don't reinvent it in panic mode.

## Consequences

- ward keeps a tight single-user posture by default. The Unix socket
  permission model + the recent SEC-002/003/004 hardening (PR #38) is
  the boundary.
- [SECURITY.md](../../SECURITY.md) explicitly lists "multi-tenant
  authn/authz" as out-of-scope for now; updates if/when this ADR is
  accepted.
- Two related issues remain open as triggers: when either lands, this
  ADR is re-opened and implementation starts.
  - [#56](https://github.com/igorjs/ward/issues/56) — this ADR's tracking issue
  - Any future "wardd for shared dev box" / "wardd at the office" use case

## Alternatives considered

- **Federation, no auth.** Each user runs their own wardd; a thin
  "router" daemon proxies to the right one. Sidesteps authn entirely
  but adds a routing layer and doesn't help cross-user sandbox
  visibility (which is sometimes desirable).
- **OS-level only (PAM, Linux capabilities, etc.).** Works but ties
  ward to host OS auth in a way that breaks macOS / Windows /
  containers without compensating value.
- **OIDC delegation to existing identity provider** (Google, Okta).
  Right answer for enterprise eventually; overkill today.
