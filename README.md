# ward

[![CI](https://github.com/igorjs/ward/actions/workflows/ci.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](Cargo.toml)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-yellow.svg)](https://github.com/users/igorjs/projects/2)

**Run untrusted code in hardware-isolated microVMs, locally.**

`ward` is a sandbox daemon that creates, manages, and destroys ephemeral
execution environments. Every sandbox boots into its own Linux microVM with
its own kernel, via [libkrun](https://github.com/containers/libkrun) on
Apple Hypervisor.framework (macOS arm64) or KVM (Linux). The isolation
boundary is hardware virtualisation, not Linux namespaces.

It's general-purpose. AI agent sandboxing, CI step isolation, "let me run
this script without it touching my home directory": ward doesn't care
about the workflow, it just runs things in isolation with first-class
egress controls and resource caps.

> **Pre-release.** v0.1.0 hasn't been cut yet. Build from source today.
> See [`docs/SPEC.md`](docs/SPEC.md) for the architecture and the
> [project board](https://github.com/users/igorjs/projects/2) for live status.

## Why ward

| Concern | Docker | E2B / Daytona (SaaS) | ward |
|---|---|---|---|
| Kernel isolation | shared host kernel | yes (cloud microVMs) | yes (local microVMs) |
| Local-first | yes | no, cloud dependency | yes |
| Egress controls | weak by default | yes | deny default + per-sandbox allowlist |
| Resource caps | yes (cgroups) | yes | per-VM CPU + memory + PID + timeout |
| Vendor lock-in | none | yes | none, AGPL daemon, open SDKs |

Docker is great at long-running services; it's wasteful and weakly
isolated for ephemeral jobs. SaaS sandboxes have strong isolation but
require sending workloads to someone else's infrastructure. ward fills
the gap: strong local isolation, simple developer UX, no cloud account.

## Quick start (developer build)

End-user installers will ship with v0.1.0. Until then, the path is
clone + `cargo build`. The default build uses a stub backend that
exercises every code path except real VM boot, so you can try the
full CLI on any platform.

### Default (stub backend)

```sh
git clone https://github.com/igorjs/ward.git
cd ward
cargo build --release
cargo test
```

Two binaries land in `target/release/`: `wardd` (the daemon) and
`ward` (the CLI).

### Real microVMs (`--features krunvm`)

Install libkrun + libkrunfw, then build with the feature flag:

**macOS Apple Silicon (12+)**

```sh
brew tap slp/krun
brew install slp/krun/libkrun slp/krun/libkrunfw
cargo build --release --features krunvm
```

**Linux (Debian/Ubuntu, kernel 5.10+ with KVM)**

```sh
# Follow https://github.com/containers/libkrun#installing
sudo apt-get install -y libkrun-dev libkrunfw-dev
cargo build --release --features krunvm
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full setup matrix and
build-time gotchas.

### End-user install (post-v0.1.0)

Once v0.1.0 is published, the one-line installer resolves the latest
release tarball, verifies the SHA-256, and installs binaries under
`~/.ward/bin/`:

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```

## Hello world

Start the daemon in one terminal:

```sh
./target/release/wardd
```

Then in another, drive it through the CLI. With the default stub backend
the operations return synthetic IDs and streams, which is enough to
explore the surface:

```sh
# Create a sandbox.
ward create alpine
# id: sb_01HXZ...
# status: creating
# image: alpine

# Exec a command inside it.
ward exec sb_01HXZ... -- echo "hello from inside"
# pid: pr_01HXZ...
# status: running

# Stream stdout/stderr of that process.
ward logs sb_01HXZ... pr_01HXZ...
# stdout: hello from inside
# exit: 0

# Tear it down.
ward remove sb_01HXZ...
# removed: sb_01HXZ...
```

The CLI surface today: `create`, `list`, `exec`, `run`, `logs`, `stdin`,
`kill`, `remove`, `snapshot {create,restore,list}`, `volume
{create,list,remove}`, `publish`, `subscribe`, `health`, `info`. Run
`ward --help` for the full reference.

## Architecture

```
                                                +------------------+
                                                |   ward (CLI)     |
                                                +--------+---------+
                                                         |
                                                         | gRPC over Unix socket
                                                         v
+---------+   pull + unpack    +---------------------------------------+
|  OCI    |  ----------------> |               wardd (daemon)           |
| images  |                    |                                        |
+---------+                    |  +----------+   +--------+   +-------+ |
                               |  |  Sandbox |   | Comms  |   | Egress| |
                               |  | Manager  |   | Broker |   | Proxy | |
                               |  +-----+----+   +---+----+   +---+---+ |
                               |        |            |            |     |
                               |        v            v            v     |
                               |  +-----------------------------------+ |
                               |  |        Backend trait               | |
                               |  |  (today: libkrun via krun_ffi)     | |
                               |  +-----+-----------------------------+ |
                               +--------|---------------------------------+
                                        |
                                        v
                              +------------------+
                              |   microVM A      |  Linux kernel
                              |   microVM B      |  Linux kernel
                              |   microVM C      |  Linux kernel
                              +------------------+
```

Per-layer rationale lives in the ADRs under [`docs/adr/`](docs/adr/).
[`docs/SPEC.md`](docs/SPEC.md) is the table of contents. Good starting
points:

- [ADR-001](docs/adr/001-project-scope.md): what's in and out of scope
- [ADR-003](docs/adr/003-isolation-backend.md): libkrun + the `krunvm` flag
- [ADR-004](docs/adr/004-ipc-protocol.md): gRPC + proto schema
- [ADR-008](docs/adr/008-egress-control.md): egress filtering model
- [ADR-011](docs/adr/011-cross-sandbox-comms.md): pub/sub broker
- [ADR-012](docs/adr/012-backend-trait.md): backend trait abstraction

## Workspace layout

```
ward-core/     Library crate: protocol types, Backend trait, libkrun FFI,
               SandboxManager, broker, image pull/unpack.
ward-daemon/   wardd binary: gRPC server over Unix socket, hosts the manager.
ward-cli/      ward binary: thin CLI client over the same gRPC.
ward-agent/    Guest-side init binary (work-in-progress; see issue #9).
proto/         ward.proto, ward_agent.proto. Single source of truth for the wire.
vendor/        Pinned libkrun version + bottle checksums.
docs/          ADRs and SPEC.md (table of contents).
scripts/       Maintenance helpers (e.g. diff-libkrun.sh).
```

## Supported platforms

| Platform | Architecture | Virtualisation | Status |
|---|---|---|---|
| macOS 12+ | Apple Silicon | Hypervisor.framework | first-class |
| Linux 5.10+ | x86_64 | KVM | first-class |
| Linux 5.10+ | arm64 | KVM | first-class |
| macOS Intel | x86_64 | n/a | not supported |
| Windows (native) | any | n/a | not supported |
| Windows (WSL2) | x86_64 | KVM via WSL2 | community-supported |

The default `cargo build` (stub backend) compiles on any platform Rust
supports. Real VM boot needs `--features krunvm` and a supported
host (see ADR-007).

## Status and roadmap

- Stub backend: complete, 387 tests passing
- libkrun FFI surface: complete (60 symbols, hand-maintained)
- VM lifecycle wiring (`krun_start_enter` + shutdown signalling): complete
- OCI image pull + unpack: complete
- Guest agent (`ward-agent`): in progress, see issue #9
- Cross-sandbox pub/sub broker: complete (deny default + group routing)
- Snapshots / volumes: API defined, backend implementation pending
- First signed release (v0.1.0): blocked on CI smoke test (issue #3)

Live status, tickets, and priorities live on the
[Ward project board](https://github.com/users/igorjs/projects/2).

## Documentation

- [CONTRIBUTING.md](CONTRIBUTING.md): dev setup, libkrun bump procedure, PR + release process
- [SECURITY.md](SECURITY.md): responsible disclosure policy
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md): community standards
- [`docs/SPEC.md`](docs/SPEC.md): ADR index
- [`proto/ward.proto`](proto/ward.proto): wire protocol

## Related repos

- [`igorjs/ward-vendor`](https://github.com/igorjs/ward-vendor): builds the
  libkrun + libkrunfw bottles that release artefacts bundle. Independent
  versioning, manual `workflow_dispatch`.
- `igorjs/ward-sdk-*` (planned): Apache-2.0 client libraries in Python,
  TypeScript, Go, Rust. Thin wrappers over the gRPC surface.

## License

`ward` itself (the daemon and CLI) is [AGPL-3.0-only](LICENSE).

The wire protocol ([`proto/ward.proto`](proto/ward.proto)) is CC0 1.0
(public domain), so SDKs can be generated and released under any licence.
Future SDKs will be Apache-2.0.

Contributing requires DCO sign-off and CLA acceptance. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the process.
