# Positioning: ward vs other libkrun-based projects

ward shares its isolation backend ([libkrun](https://github.com/containers/libkrun))
with several adjacent projects. This page is an honest comparison so you
can pick the right tool for your problem instead of choosing on vibes.

## ward vs microsandbox

[microsandbox](https://github.com/superradcompany/microsandbox) is an
Apache-2.0 Rust project that, like ward, boots libkrun microVMs from
OCI images. It is more mature today (beta, ~6k GitHub stars at the time
of writing) and has shipped SDKs in four languages.

The projects make **opposite architectural bets**:

| Dimension                  | microsandbox                  | ward                                           |
|----------------------------|-------------------------------|------------------------------------------------|
| Daemon                     | None — embedded per process   | `wardd` (long-running gRPC service)            |
| Primary integration        | SDK in your application       | Daemon + CLI + MCP + SDK clients               |
| Networking                 | Userspace (`smoltcp`)          | TAP devices (today), `smoltcp` planned         |
| Rootless                   | Yes                           | Daemon assumes a privileged install today      |
| Cross-sandbox messaging    | Not a core feature            | First-class pub/sub broker (audited, policied) |
| Observability              | Logs                          | Prometheus `/metrics`, structured tracing      |
| Multi-tenancy              | Single process owns sandboxes | Daemon brokers many tenants, policy at hub     |
| MCP for agents             | First-party SDK + skills      | `ward-mcp` stdio server (this repo)            |
| License                    | Apache-2.0 throughout         | AGPL-3.0 (daemon/runtime), Apache-2.0 (SDKs)   |

### Pick microsandbox if…

- You want a library-first developer experience: `cargo add microsandbox`
  + `Sandbox::builder(...).create()` and no infrastructure.
- Your workload is one process owning a handful of sandboxes for its own
  use (LLM tool execution, CI step isolation inside a single test run).
- You don't need cross-sandbox communication or fleet-level observability.

### Pick ward if…

- You're running sandboxes as part of a multi-tenant service and need a
  control plane: gRPC API surface, Prometheus metrics, per-tenant policy.
- You need an audited cross-sandbox pub/sub bus where the broker is the
  policy enforcement point.
- You want first-class MCP integration with stdio transport that agents
  (Claude / Cursor / Codex) can spawn directly.
- You're willing to accept the AGPL boundary on the runtime in exchange
  for the fleet posture.

### Where they could converge

ward's [ADR-016](adr/016-embedded-mode-microvms.md) considers adopting
the microsandbox-style embedded mode (and `smoltcp` networking) as a
*second* runtime mode alongside the daemon. Whether that lands in ward
depends on ADR-017's license decision (the SDK can't link AGPL runtime
without re-licensing). Until then, ward's "embedded" use case is
`ward-mcp` — an MCP server binary that owns its own runtime.

## ward vs Firecracker / firecracker-containerd

Firecracker is AWS's microVM monitor; libkrun is the spiritual successor
on macOS via Apple's Hypervisor.framework. firecracker-containerd is the
daemon you'd use to actually orchestrate Firecracker VMs at fleet scale.

ward is structurally similar to firecracker-containerd's role — a daemon
that brokers microVM lifecycle for many clients — but built on libkrun
instead of Firecracker. The library/daemon-of-many-tenants pattern is
where ward gets its design inspiration from, not the embedded-SDK
projects. See [ADR-003](adr/003-isolation-backend.md) for why ward picked
libkrun over Firecracker.

## ward vs Docker / gVisor / Kata

This comparison is in [docs/why.md](why.md) — it's about isolation
*technique* (microVM vs namespace vs ptrace vs Kata's runv) rather than
project posture, so it belongs there.
