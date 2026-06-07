# ward

[![CI](https://github.com/igorjs/ward/actions/workflows/ci.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/ci.yml)
[![cargo-audit](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/igorjs/ward/badge)](https://scorecard.dev/viewer/?uri=github.com/igorjs/ward)
[![SLSA Level 3](https://slsa.dev/images/gh-badge-level3.svg)](https://slsa.dev/spec/v1.0/levels)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](Cargo.toml)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-yellow.svg)](https://github.com/users/igorjs/projects/2)

**A microVM control plane for sandboxed execution — fleet-grade, observable, MCP-ready.**

`ward` boots each workload into its own Linux microVM via
[libkrun](https://github.com/containers/libkrun) (Apple
Hypervisor.framework on macOS arm64, KVM on Linux). The isolation boundary
is hardware virtualisation, not Linux namespaces. Three integration paths:

- **Daemon (`wardd`)** — long-running gRPC service with Prometheus metrics,
  cross-sandbox pub/sub broker, egress policy, multi-tenant resource caps.
  Designed for fleet operation and observability.
- **MCP server (`ward-mcp`)** — exposes sandboxed execution as MCP tools
  for LLM agents (Claude, Cursor, Codex). Embedded; no daemon required.
- **SDKs** — Apache-2.0 clients in Rust (Python / TS / Go scaffolded);
  generated from [`proto/ward.proto`](proto/ward.proto) (CC0).

See [`docs/why.md`](docs/why.md) for the Docker / SaaS comparison, and
[`docs/positioning.md`](docs/positioning.md) for how ward differs from
other libkrun-based projects.

> **Pre-release.** v0.1.0 hasn't been cut yet. Build from source today;
> live status on the [project board](https://github.com/users/igorjs/projects/2).

## Quick start

```sh
git clone https://github.com/igorjs/ward.git
cd ward
cargo build --release   # default stub backend, any platform
cargo test
```

Workspace binaries:

| Binary       | Purpose                                                          |
|--------------|------------------------------------------------------------------|
| `wardd`      | Long-running daemon (gRPC over Unix socket)                       |
| `ward`       | CLI for the daemon                                                |
| `ward-mcp`   | MCP stdio server for LLM agents                                   |

Real microVMs need `--features krunvm` plus libkrun installed — see
[`docs/platforms.md`](docs/platforms.md).

## Hello world (daemon + CLI)

```sh
./target/release/wardd &                       # start the daemon
ward create alpine                             # → sandbox id
ward exec <id> -- echo "hello from inside"
ward logs <id> <pid>
ward remove <id>
```

## Hello world (MCP, for agents)

Drop into a Claude / Cursor MCP config:

```json
{
  "mcpServers": {
    "ward": {
      "command": "/path/to/ward-mcp",
      "env": { "WARD_DATA_DIR": "/home/you/.ward/data" }
    }
  }
}
```

The agent can then call `ward_create_sandbox`, `ward_list_sandboxes`,
`ward_exec`, and `ward_remove_sandbox` directly.

Full CLI: `ward --help`.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — architecture index (ADRs)
- [`docs/architecture.md`](docs/architecture.md) — system diagram
- [`docs/platforms.md`](docs/platforms.md) — supported platforms + libkrun setup
- [`docs/workspace.md`](docs/workspace.md) — crate layout
- [`docs/status.md`](docs/status.md) — what's shipped, what's pending
- [`docs/why.md`](docs/why.md) — why ward vs Docker / SaaS sandboxes
- [`docs/adr/`](docs/adr/) — architecture decision records (most recent: [ADR-016](docs/adr/016-embedded-mode-microvms.md))
- [CONTRIBUTING.md](CONTRIBUTING.md) — dev setup, libkrun bump, PR + release
- [SECURITY.md](SECURITY.md) — responsible disclosure
- [`proto/ward.proto`](proto/ward.proto) — wire protocol (CC0)

## Related

- [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds) — libkrun
  bottles bundled by release artefacts.
- [`sdks/`](sdks/) — Apache-2.0 client libraries (Rust shipping, Python / TS / Go
  scaffolded) generated from `proto/ward.proto`.
- [`superradcompany/microsandbox`](https://github.com/superradcompany/microsandbox)
  — adjacent libkrun-based runtime focused on embedded-SDK use; ward
  bets on the fleet/daemon angle. See
  [`docs/positioning.md`](docs/positioning.md) for the comparison.

## License

[AGPL-3.0-only](LICENSE) for the daemon, CLI, runtime, and MCP server.
The wire protocol ([`proto/ward.proto`](proto/ward.proto)) is CC0; SDKs
are Apache-2.0 (compiled from the proto, no AGPL linkage — see
[ADR-016](docs/adr/016-embedded-mode-microvms.md)). Contributing requires
DCO sign-off and CLA — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
process.
