# Contributing to Ward

Thank you for your interest in contributing. This document explains the process and requirements.

## Before You Start

1. **Check existing issues** to see if someone is already working on what you want to change.
2. **Open an issue first** for significant changes (new modules, API changes, architecture). Small fixes and documentation improvements can go straight to a PR.
3. **Read the [Code of Conduct](CODE_OF_CONDUCT.md)**.

## Requirements

Every contribution must satisfy two legal requirements:

### 1. Developer Certificate of Origin (DCO)

All commits must include a `Signed-off-by` trailer certifying that you have the right to submit the code. Add it with:

```bash
git commit --signoff -m "your commit message"
```

This adds a line like:

```
Signed-off-by: Your Name <your@email.com>
```

The DCO bot will check every commit in your PR. If any commit is missing the trailer, the bot will comment with instructions.

### 2. Contributor License Agreement (CLA)

First-time contributors must sign a CLA. This grants the project a license to use your contribution and protects both you and the project.

**Individual contributors:** Sign the [Individual CLA](.github/ICLA.md) by commenting on your first PR with:

```
I have read the CLA Document and I hereby sign the CLA.
```

The CLA bot will record your signature automatically. You only need to do this once across all repositories maintained by igorjs.

**Corporate contributors:** If you are contributing on behalf of your employer, your organisation must sign the [Corporate CLA](.github/CCLA.md). Email the signed document to oss@mail.igorjs.io. Individual employees listed as Designated Employees do not need to sign the Individual CLA separately.

## Development

### Prerequisites

- **Rust** (latest stable, install via [rustup](https://rustup.rs/))
- **macOS arm64** or **Linux** (x86_64 / arm64). Intel Macs aren't currently
  supported because libkrun's hypervisor backend uses Apple Silicon's HVF.

### Setup

The default build works with no extra setup — the backend ships a
stub mode that exercises the full code path without a real microVM:

```bash
git clone https://github.com/igorjs/ward.git
cd ward
cargo build
cargo test
```

### Setup with real microVMs (libkrun)

If you want `wardd` to boot actual microVMs (i.e. build with
`--features krunvm`), you need libkrun and libkrunfw on the system.
The release artefacts we publish to end users bundle these dylibs
inside the binary's rpath — see `vendor/libkrun-build/README.md` — but
developer builds rely on the system package manager:

**macOS Apple Silicon**

```bash
brew tap slp/krun
brew install slp/krun/libkrun slp/krun/libkrunfw
cargo build --features krunvm
```

**Linux (Debian/Ubuntu)**

```bash
# Add the libkrun apt repo per upstream instructions:
# https://github.com/containers/libkrun#installing
sudo apt-get install -y libkrun-dev libkrunfw-dev
cargo build --features krunvm
```

Why this isn't bundled at `cargo build` time: an attempt to vendor
libkrun via `ward-core/build.rs` ran into a structural cargo issue
(dependency build scripts run before dependents, so the build.rs
couldn't prepare the environment for `krun-sys`). The "users install
nothing but ward" promise is satisfied by *end-user release artefacts*
(`.pkg`, `.deb`, install.sh) that bundle the dylibs — see
`vendor/libkrun-build/` for the artefact production pipeline.

### Workflow

```bash
cargo fmt --check   # format check
cargo clippy        # lint
cargo build         # build
cargo test          # unit tests
```

### Code Style

- All source files must start with `// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only`
- Follow existing patterns and Rust idioms
- Use `Result` and `Option` instead of panicking
- Prefer zero-copy APIs where possible
- Keep unsafe blocks minimal and documented

### Commits

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(sandbox): add snapshot support
fix(network): correct veth cleanup on failure
docs: update installation guide
test: add egress proxy integration tests
```

Always sign commits: `git commit --signoff --gpg-sign`

## Pull Request Process

1. Fork the repository and create a branch from `main`.
2. Make your changes with tests.
3. Ensure all checks pass: `cargo fmt --check && cargo clippy && cargo test`
4. Sign the CLA (first-time only).
5. Submit a PR with a clear description of what and why.
6. Address review feedback.

## Reporting Bugs

Open a GitHub issue with:
- Rust version and target triple
- Operating system and kernel version
- Minimal reproduction steps
- Expected vs actual behaviour
- Error messages (full output)

## Security Vulnerabilities

Do **not** open a public issue for security vulnerabilities. See [SECURITY.md](SECURITY.md) for responsible disclosure instructions.

## License

By contributing, you agree that your contributions will be licensed under the [GNU Affero General Public License v3.0](LICENSE), subject to the terms of the [CLA](.github/ICLA.md).
