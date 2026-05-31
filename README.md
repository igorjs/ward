# ward

[![CI](https://github.com/igorjs/ward/actions/workflows/ci.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/ci.yml)
[![cargo-audit](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml/badge.svg)](https://github.com/igorjs/ward/actions/workflows/cargo-audit.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/igorjs/ward/badge)](https://scorecard.dev/viewer/?uri=github.com/igorjs/ward)
[![SLSA Level 3](https://slsa.dev/images/gh-badge-level3.svg)](https://slsa.dev/spec/v1.0/levels)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](Cargo.toml)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-yellow.svg)](https://github.com/users/igorjs/projects/2)

**Run untrusted code in hardware-isolated microVMs, locally.**

`ward` is a sandbox daemon that boots each request into its own Linux
microVM via [libkrun](https://github.com/containers/libkrun) (Apple
Hypervisor.framework on macOS arm64, KVM on Linux). The isolation
boundary is hardware virtualisation, not Linux namespaces — with
first-class egress controls and resource caps. General-purpose: AI agent
sandboxing, CI step isolation, ad-hoc script execution.

See [`docs/why.md`](docs/why.md) for the Docker / SaaS comparison.

> **Pre-release.** v0.1.0 hasn't been cut yet. Build from source today;
> live status on the [project board](https://github.com/users/igorjs/projects/2).

## Quick start

```sh
git clone https://github.com/igorjs/ward.git
cd ward
cargo build --release   # default stub backend, any platform
cargo test
```

Two binaries land in `target/release/`: `wardd` (daemon) and `ward` (CLI).
Real microVMs need `--features krunvm` plus libkrun installed — see
[`docs/platforms.md`](docs/platforms.md).

## Hello world

```sh
./target/release/wardd &                  # start the daemon
ward create alpine                         # → sandbox id
ward exec <id> -- echo "hello from inside"
ward logs <id> <pid>
ward remove <id>
```

Full CLI: `ward --help`.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — architecture index (ADRs)
- [`docs/architecture.md`](docs/architecture.md) — system diagram
- [`docs/platforms.md`](docs/platforms.md) — supported platforms + libkrun setup
- [`docs/workspace.md`](docs/workspace.md) — crate layout
- [`docs/status.md`](docs/status.md) — what's shipped, what's pending
- [`docs/why.md`](docs/why.md) — why ward vs Docker / SaaS sandboxes
- [CONTRIBUTING.md](CONTRIBUTING.md) — dev setup, libkrun bump, PR + release
- [SECURITY.md](SECURITY.md) — responsible disclosure
- [`proto/ward.proto`](proto/ward.proto) — wire protocol (CC0)

## Related

- [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds) — libkrun
  bottles bundled by release artefacts.
- [`sdks/`](sdks/) — Apache-2.0 client libraries (Python, TypeScript, Go,
  Rust) generated from `proto/ward.proto`.

## License

[AGPL-3.0-only](LICENSE) for the daemon and CLI. The wire protocol
([`proto/ward.proto`](proto/ward.proto)) is CC0; SDKs are Apache-2.0.
Contributing requires DCO sign-off and CLA — see
[CONTRIBUTING.md](CONTRIBUTING.md) for the process.
