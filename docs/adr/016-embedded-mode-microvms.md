# ADR-016: Embedded-Mode microVMs (Daemonless + Rootless + Userspace Networking)

**Status:** Accepted
**Date:** 2026-06-04
**Authors:** Igor

## Context

Ward's architecture to date assumes a long-running daemon (`wardd`) that owns libkrun, brokers cross-sandbox messages, and serves gRPC over a Unix socket. SDKs and the CLI are thin clients that connect to that daemon. ADR-003 (libkrun), ADR-004 (gRPC), ADR-005 (SDK strategy), and ADR-012 (Backend trait) all assume this shape.

In mid-2026 the [microsandbox project](https://github.com/superradcompany/microsandbox) (Apache 2.0, ~6.4k stars) made a different bet on the same backend: **no daemon**. The SDK boots libkrun as a child process, owns its lifecycle, and exits when the sandbox does. They use [`smoltcp`](https://github.com/smoltcp-rs/smoltcp) (userspace TCP/IP) instead of TAP devices, which lets the whole stack run **rootless**. They ship MCP integration and SDKs in four languages.

This raises three questions ward has not previously answered:

1. **Daemon or embedded?** Should the SDK require a running `wardd`, or boot libkrun in-process? Microsandbox proves the embedded model works; ward's daemon was an unforced default, not a researched decision.
2. **TAP or smoltcp?** Backlog issue [#32](https://github.com/igorjs/ward/issues/32) plans network publishing via `krun_add_net_tap`. TAP devices require `CAP_NET_ADMIN` (i.e. root). smoltcp runs entirely in userspace.
3. **Rootful or rootless?** Ward currently expects a privileged install (`install.sh` may need sudo on Linux to set up TAP / KVM perms). Rootless is required for the embedded SDK story to be meaningful — users will not `sudo pip install ward-sdk`.

These three questions are not independent:

- A daemon can stay rootful (it's a one-time install); an embedded SDK cannot.
- TAP networking requires root; smoltcp does not.
- Embedded mode without rootless is pointless. Smoltcp without embedded is over-engineering.

Treating them as one decision is honest. That's why this ADR bundles them.

### Refactor cost (measured, not guessed)

An audit of ward-core surface area (2026-06-04) found:

- `SandboxManager` (1744 lines): one hard dependency on `Broker` (logical, not daemon-specific). No tonic/gRPC types leak in. Mockable.
- `Broker` (~250 lines): pure tokio primitives. Self-contained. Can be conditionally absent.
- `ward-daemon/src/main.rs` (263 lines): ~40 lines of daemon-only setup (metrics exporter, signal handlers, socket binding). The rest is reusable init.
- `WardGrpcServer`: 5-line shim over `Arc<SandboxManager>` + `Arc<VolumeManager>`.
- `ward-cli/`: 100% gRPC-coupled. Needs a parallel transport path for embedded.
- `#[cfg(feature = "krunvm")]`: 22 call sites, all concentrated in `ward-core/src/backend/`.

The Backend trait abstraction (ADR-012) did exactly what it was supposed to: nothing in the manager, broker, or backend assumes a daemon. The refactor surface is the CLI transport layer and a daemon-init extraction.

**Estimated cost: 1–2 weeks of focused work, not 4–6.**

## Decision

Ward adopts **embedded mode** as a first-class runtime alongside the existing daemon. The CLI default flips to embedded; the daemon becomes opt-in. Networking moves to smoltcp. Both modes run rootless on supported platforms.

### 1. Two runtime modes, one Backend trait

```
┌─────────────────────────┐         ┌─────────────────────────┐
│ Embedded mode (default) │         │ Daemon mode (opt-in)    │
│                         │         │                         │
│  Application / CLI      │         │  Client (CLI, SDK)      │
│         │               │         │         │ gRPC over UDS │
│         ▼               │         │         ▼               │
│  ward-runtime (lib)     │         │  wardd                  │
│     SandboxManager      │         │     SandboxManager      │
│     Backend             │         │     Backend             │
│     libkrun             │         │     libkrun             │
└─────────────────────────┘         └─────────────────────────┘
```

Both modes instantiate the same `SandboxManager` and the same `Backend`. The difference is *who owns the process* and *how the client reaches the manager*:

- **Embedded:** SDK / CLI links `ward-runtime` and calls manager methods directly (in-process function calls). The libkrun thread (per ADR-009) lives in the calling process. Process exits when the application exits.
- **Daemon:** SDK / CLI talks gRPC over UDS / TCP to `wardd`, which owns the manager. `wardd` outlives any client.

### 2. Mode selection

```rust
// Default: embedded
let sandbox = Sandbox::builder("my-sandbox")
    .image("alpine")
    .create()
    .await?;

// Opt into the daemon
let sandbox = Sandbox::builder("my-sandbox")
    .daemon("unix:///run/ward/ward.sock")  // or "tcp://host:port"
    .image("alpine")
    .create()
    .await?;
```

The CLI follows the same convention:

```bash
ward create alpine                          # embedded
WARD_DAEMON_ADDR=$XDG_RUNTIME_DIR/ward/ward.sock ward create alpine   # daemon
```

This matches the Docker mental model (users don't think about `dockerd` until they need to), and is what microsandbox-style users expect.

### 3. `ward-runtime` crate

A new crate, `ward-runtime`, exports the embedded entry point. It wraps the existing `SandboxManager` + `Backend` + `Broker` construction logic that today lives in `ward-daemon/src/main.rs` lines ~131–151.

```rust
// ward-runtime/src/lib.rs
pub struct Runtime { mgr: Arc<SandboxManager>, /* ... */ }

impl Runtime {
    pub fn builder() -> RuntimeBuilder { /* ... */ }
}

impl RuntimeBuilder {
    pub fn data_dir(self, path: PathBuf) -> Self { /* ... */ }
    pub fn max_sandboxes(self, n: usize) -> Self { /* ... */ }
    pub async fn build(self) -> Result<Runtime> { /* ... */ }
}
```

`wardd` becomes a ~50-line `main.rs` that builds a `Runtime`, wraps `WardGrpcServer` around its manager, and serves. The SDK boots a `Runtime` per process and never sees gRPC.

### 4. Broker: required, in-process

The audit found `Broker::register_sandbox` on the hot path of `create_sandbox` (manager.rs:254). Rather than gate Broker behind a feature, embedded mode keeps it: cross-sandbox pub/sub still works *within* a process. Cross-process pub/sub remains a daemon-mode capability.

This is the cleanest split: embedded = single-tenant single-process; daemon = multi-tenant multi-process.

### 5. Networking: smoltcp, not TAP

Issue #32 (port publishing via `krun_add_net_tap`) is closed in favour of:

- Guest VM: virtio-net device (libkrun already supports this).
- Host: a userspace TCP/IP stack via `smoltcp` that terminates the guest's traffic and bridges to the host's normal sockets.

Why:

- TAP requires `CAP_NET_ADMIN` on Linux. smoltcp requires nothing.
- TAP needs root or admin-managed setup. smoltcp runs as a normal process.
- Port publishing becomes a userspace forward rule, not an iptables-equivalent dance.
- Same code path on macOS and Linux. TAP on macOS requires utun + extra plumbing.

Trade-off: smoltcp does not give the guest a "real" Linux TCP/IP stack viewable on `ip addr`. For ward's workload (sandboxed processes, not full network appliances) this is fine. If a future use case demands TAP we add it behind a feature flag.

### 6. Rootless

**macOS (Apple Silicon):** libkrun runs unprivileged with the Hypervisor entitlement. `install.sh` signs the binary with the entitlement; no sudo needed.

**Linux:** Two routes, both rootless:

- KVM via the user's `kvm` group membership. `install.sh` documents the one-time `usermod -aG kvm $USER` step but does not perform it (user runs it themselves).
- User namespaces for filesystem and process isolation. No `setuid` binary.

`/usr/local/bin/wardd` (or its embedded equivalent) is installed by the user, into the user's prefix, owned by the user. `~/.ward/` is the data directory.

### 7. SDK strategy update (amends ADR-005)

ADR-005 anticipated SDKs as gRPC clients. Embedded mode means SDKs are now:

- **Rust SDK:** depends on `ward-runtime` (embedded) + generated tonic client (daemon). User picks at builder time.
- **Python / TS / Go SDKs:** wrap a small native helper (Rust binary) for embedded mode; fall back to gRPC client for daemon mode. The native helper is what microsandbox-style "no infrastructure required" means in practice.

The protobuf-as-source-of-truth principle from ADR-004 still holds: the daemon RPC surface is the same regardless of mode. Embedded mode just bypasses the wire.

### 8. MCP

ADR is silent on MCP — it's a separate ADR (017) and a separate crate (`ward-mcp`). MCP wraps the Rust SDK, so it inherits whichever mode the user configured.

## Consequences

### Positive

- **Onboarding parity with microsandbox.** `cargo add ward` + `Sandbox::builder(...).create()` works without any daemon install. Removes the biggest friction in the existing story.
- **Rootless on both platforms.** No sudo in install.sh. No `CAP_NET_ADMIN`. Required for SDK distribution via pip / npm / crates.io.
- **Daemon stays for fleet use.** Multi-process pub/sub, observability (Prometheus, #77), policy enforcement, multi-tenant auth (ADR-013) — all daemon-only by design. This is ward's differentiator and we lean into it.
- **Backend trait justified retroactively.** ADR-012's "cheap insurance" was actually a load-bearing decision; the embedded refactor is 1–2 weeks instead of 4–6 because of it.
- **Issue #32 closes; new smoltcp work scoped.** Net work avoided.

### Negative

- **Two runtime paths to test.** Embedded and daemon. CI matrix doubles for end-to-end tests (though unit tests share).
- **smoltcp is a new dependency we have to learn.** It is well-maintained but more complex than `ip tuntap`. Risk concentrated in one crate.
- **Embedded mode means per-process libkrun.** Two processes that each want a sandbox cannot share libkrun state. For most agentic / scripting use cases this is fine; for "fleet" use cases users must use daemon mode (which is correct).
- **`install.sh` rewrites.** SLSA L3 release flow assumes a single artifact; embedded vs daemon may mean two install paths. (Probably one binary with subcommands, but worth verifying.)
- **License boundary blocks the obvious SDK embedding path.** `ward-core` and `ward-runtime` are AGPL-3.0; `sdks/rust/ward-client` is Apache-2.0 by design (so library users aren't infected). A `path = "../ward-runtime"` dep from the SDK would transitively pull AGPL code into an Apache-2.0 crate, which silently relicenses it. Resolution paths, in order of preference:
  1. **Helper-binary embedded mode** — SDK spawns a small AGPL helper (`ward-embed-helper`) that hosts a Runtime + an in-stdio gRPC server. SDK talks to its own helper. AGPL boundary stays clean. Matches microsandbox's "ship the runtime, talk over a private channel" pattern.
  2. **`ward-proto` crate (Apache-2.0)** — extract the protobuf-generated types into a standalone Apache-2.0 crate so SDKs and tooling can depend on the wire surface without AGPL linkage. Already foreshadowed in `sdks/rust/ward-client/Cargo.toml`.
  3. **Relicense `ward-core` / `ward-runtime`** — separate ADR (017). Not in scope here.
- **CLI cannot meaningfully run embedded for stateful commands.** `ward create alpine` followed by `ward exec <id> -- ...` requires the sandbox to outlive the first CLI process. Embedded means the libkrun thread dies with the CLI. So:
  - SDK and `ward-mcp`: embedded is the right default.
  - CLI: stays daemon-only for stateful commands (create, exec, list, snapshot, volume). A future `ward run --embedded <image> -- <cmd>` one-shot mode is a reasonable follow-up but is *not* a default flip. **The "CLI default flips to embedded" claim earlier in this ADR is wrong** and is corrected here. The Docker analogy doesn't apply — `docker run` is using a daemon, not running embedded.

### Neutral

- **Embedded mode does not subsume daemon.** Both exist. Users with policy / observability / multi-tenant needs choose daemon.
- **CLI default flips, which is a user-visible change.** v0.1.0 ships with the embedded default; documentation must be clear about when to use `WARD_DAEMON_ADDR`.

## Implementation order

Revised after surfacing the license constraint and the CLI/stateful-sandbox
mismatch above:

1. ✅ **`ward-runtime` crate** — extracted from daemon `main.rs` init; daemon now consumes it.
2. **Complete Rust SDK gRPC client** — wire up the `unimplemented!` methods in `sdks/rust/ward-client` against the daemon's socket. This is the immediate value-add and ships at v0.1.
3. **`ward-mcp` crate (AGPL)** — server binary, depends on `ward-runtime` directly. Embedded mode lives here because an MCP server *is* a per-process owner of its sandboxes. Resolves the "first-class agent integration" goal without crossing the license boundary.
4. **ADR-017: license posture** — decide between (a) helper-binary embedded SDK, (b) `ward-proto` crate extraction, or (c) relicensing. Until then, embedded SDK is parked.
5. **smoltcp networking spike** (replaces #32) — independent of the above; can run in parallel.
6. **Rootless install.sh** — macOS Hypervisor entitlement + Linux KVM group documentation.
7. **README repositioning** — ward is "fleet daemon + thin SDK + MCP for agents", not "microsandbox-clone."
8. **Python / TS / Go SDKs** — gRPC-only initially; embedded mode awaits ADR-017.

Steps 1–3 + 7 are the v0.1 critical path. Steps 4–6, 8 follow.

## References

- [microsandbox](https://github.com/superradcompany/microsandbox) — prior art for daemonless libkrun
- [smoltcp](https://github.com/smoltcp-rs/smoltcp) — userspace TCP/IP stack
- ADR-003: Isolation backend (libkrun)
- ADR-004: IPC protocol (gRPC) — unchanged for daemon mode
- ADR-005: SDK strategy — amended by this ADR
- ADR-012: Backend trait — load-bearing, retroactively justified
- Issue #32: closes in favour of smoltcp work
