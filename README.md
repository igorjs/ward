# ward

[![CI](https://github.com/igorjs/ward/actions/workflows/ci.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/ci.yml)
[![cargo-audit](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/igorjs/ward/badge)](https://scorecard.dev/viewer/?uri=github.com/igorjs/ward)
[![SLSA Level 3](https://slsa.dev/images/gh-badge-level3.svg)](https://slsa.dev/spec/v1.0/levels)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](Cargo.toml)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-yellow.svg)](https://github.com/users/igorjs/projects/2)

**A microVM control plane for sandboxed execution; fleet-grade, observable, MCP-ready.**

`ward` boots each workload into its own Linux microVM via
[libkrun](https://github.com/containers/libkrun) (Apple
Hypervisor.framework on macOS arm64, KVM on Linux). The isolation boundary
is hardware virtualisation, not Linux namespaces. Three integration paths:

- **Daemon (`wardd`)**; long-running gRPC service with Prometheus metrics,
  cross-sandbox pub/sub broker, egress policy, multi-tenant resource caps.
  Designed for fleet operation and observability.
- **MCP server (`ward-mcp`)**; exposes sandboxed execution as MCP tools
  for LLM agents (Claude, Cursor, Codex). Embedded; no daemon required.
- **SDKs**; Apache-2.0 clients in Rust (Python / TS / Go scaffolded);
  generated from [`proto/ward.proto`](proto/ward.proto) (CC0).

See [`docs/why.md`](docs/why.md) for the Docker / SaaS comparison, and
[`docs/positioning.md`](docs/positioning.md) for how ward differs from
other libkrun-based projects.

> **Pre-release.** v0.1.0 is the first signed semver release; see the
> [project board](https://github.com/users/igorjs/projects/2) for live status.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```

Installs `ward`, `wardd`, and `ward-mcp` to `~/.ward/bin`. Tarballs are
SHA-256 pinned and SLSA Build L3 attested; if `slsa-verifier` is on `PATH`
the installer checks the provenance chain before extracting. On macOS the
daemon ships codesigned with the Hypervisor entitlement; on Linux you'll
need to be in the `kvm` group (`sudo usermod -aG kvm $USER`). See
[`docs/platforms.md`](docs/platforms.md) for the full per-platform setup.

Workspace binaries:

| Binary       | Purpose                                                          |
|--------------|------------------------------------------------------------------|
| `wardd`      | Long-running daemon (gRPC over Unix socket)                       |
| `ward`       | CLI for the daemon                                                |
| `ward-mcp`   | MCP stdio server for LLM agents (embedded; no daemon required)    |

## Build from source

```sh
git clone https://github.com/igorjs/ward.git
cd ward

# macOS (Apple Silicon)
brew install slp/krun/libkrun slp/krun/libkrunfw
# Linux (Debian/Ubuntu)
sudo apt-get install -y libkrun-dev libkrunfw-dev

cargo build --release --features krunvm
cargo test --features krunvm
```

Release binaries always ship with `--features krunvm` and a bundled
libkrun bottle from [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds);
the installer above does this for you. The default `cargo build`
(no features) compiles against a stub backend on any platform; useful
for contributors hacking on broker/CLI/MCP code without libkrun
installed, not for actually running sandboxes. See
[`docs/platforms.md`](docs/platforms.md) for the full setup matrix.

## Hello world (daemon + CLI)

```sh
wardd &                                  # start the daemon
ward create alpine                       # → sandbox id
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

- [`docs/SPEC.md`](docs/SPEC.md); architecture index (ADRs)
- [`docs/architecture.md`](docs/architecture.md); system diagram
- [`docs/platforms.md`](docs/platforms.md); supported platforms + libkrun setup
- [`docs/workspace.md`](docs/workspace.md); crate layout
- [`docs/status.md`](docs/status.md); what's shipped, what's pending
- [`docs/why.md`](docs/why.md); why ward vs Docker / SaaS sandboxes
- [`docs/adr/`](docs/adr/); architecture decision records (most recent: [ADR-016](docs/adr/016-embedded-mode-microvms.md))
- [CONTRIBUTING.md](CONTRIBUTING.md); dev setup, libkrun bump, PR + release
- [SECURITY.md](SECURITY.md); responsible disclosure
- [`proto/ward.proto`](proto/ward.proto); wire protocol (CC0)

## Related

- [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds); libkrun
  bottles bundled by release artefacts.
- [`sdks/`](sdks/); Apache-2.0 client libraries (Rust shipping, Python / TS / Go
  scaffolded) generated from `proto/ward.proto`.
- [`superradcompany/microsandbox`](https://github.com/superradcompany/microsandbox)
 ; adjacent libkrun-based runtime focused on embedded-SDK use; ward
  bets on the fleet/daemon angle. See
  [`docs/positioning.md`](docs/positioning.md) for the comparison.

## License

[AGPL-3.0-only](LICENSE) for the daemon, CLI, runtime, and MCP server.
The wire protocol ([`proto/ward.proto`](proto/ward.proto)) is CC0; SDKs
are Apache-2.0 (compiled from the proto, no AGPL linkage; see
[ADR-016](docs/adr/016-embedded-mode-microvms.md)). Contributing requires
DCO sign-off and CLA; see [CONTRIBUTING.md](CONTRIBUTING.md) for the
process.
