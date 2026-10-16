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

## Architecture

Before making non-trivial changes, skim the ADRs under [`docs/adr/`](docs/adr/). [`docs/SPEC.md`](docs/SPEC.md) is the table of contents. Each ADR is short (~50–100 lines) and explains *why* a piece of the system is shaped the way it is. Particularly useful starting points:

- [ADR-001](docs/adr/001-project-scope.md) — what's in/out of scope
- [ADR-003](docs/adr/003-isolation-backend.md) — libkrun and the `krunvm` feature flag
- [ADR-004](docs/adr/004-ipc-protocol.md) — gRPC + proto schema source of truth
- [ADR-011](docs/adr/011-cross-sandbox-comms.md) — the pub/sub broker
- [ADR-012](docs/adr/012-backend-trait.md) — the `Backend` trait abstraction

## Development

### Prerequisites

- **Rust** (latest stable, install via [rustup](https://rustup.rs/))
- **macOS arm64** or **Linux** (x86_64 / arm64). Intel Macs aren't currently supported because libkrun's hypervisor backend uses Apple Silicon's HVF.

### Setup

The default build works with no extra setup — the backend ships a stub mode that exercises the full code path without a real microVM:

```bash
git clone https://github.com/igorjs/ward.git
cd ward
cargo build
cargo test
```

### Setup with real microVMs

Nothing has to be installed. Build with the real backend:

```bash
cargo build --features ward-core/krunvm,ward-daemon/krunvm
```

The `ward-bundle` crate downloads the pinned libkrun release (checked against `vendor/libkrun-checksums.txt`), cross-compiles the in-guest agent for Linux, and embeds both. You need `rustup` and network access once. Set `WARD_LIBKRUN_TARBALL=/path/to/tarball` to build offline. On macOS, sign `wardd` and `ward-mcp` after each build (see "macOS codesign" below).

The real-VM end-to-end tests need KVM on Linux or Hypervisor.framework on macOS:

```bash
cargo build -p ward-daemon -p ward-cli --features ward-daemon/krunvm
WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm
```

FFI declarations live in `ward-core/src/backend/krun_ffi.rs` (hand-maintained, loaded with `dlopen`, no `krun-sys` crate and no bindgen).

### Bumping libkrun

Ward pins a specific libkrun version in `vendor/libkrun-version.txt` and declares the C ABI by hand in `ward-core/src/backend/krun_ffi.rs`. When upstream releases a new version that ward should adopt, follow this procedure end-to-end. The same convention lives in [ADR-003 Update section](docs/adr/003-isolation-backend.md#update--2026-05-18).

**1. Diff the header.** Run the helper script with the target version:

```bash
scripts/diff-libkrun.sh <new-version>
# e.g. scripts/diff-libkrun.sh 1.19.0
```

It fetches `include/libkrun.h` for the currently-pinned version (from `vendor/libkrun-version.txt`) and the new version, and prints a unified diff. To list only added function declarations:

```bash
scripts/diff-libkrun.sh 1.19.0 | grep -E '^\+(int32_t|uint32_t|void) krun_'
```

**2. Translate new signatures.** For each added declaration in the diff, add a matching `unsafe extern "C"` line in `ward-core/src/backend/krun_ffi.rs`. Group by the existing section comments (Context lifecycle, VM config, Networking, GPU/display, Audio, Resource limits, Exec config, Firmware/kernel, TEE, vsock, Console/serial, Virt features, Logging, Shutdown signalling, Boot). Use the smallest correct Rust types: `int32_t` -> `i32`, `uint8_t` -> `u8`, `const char *` -> `*const c_char`, NUL-terminated `char **` -> `*const *const c_char`. Watch for `uint8_t` masquerading as a count field (silent truncation if Rust callers pass a larger type; surface as an error per the `krun_set_vm_config` precedent).

**3. Remove deletions.** If any function disappeared upstream, remove its declaration here. Check libkrun's `ABI_VERSION` constant in upstream `Makefile` is unchanged across the bump; if it incremented, the SO version changed and we need a coordinated release rather than a drop-in bump.

**4. Bump the pin and trigger a rebuild.** Edit `vendor/libkrun-version.txt` to the new version. If libkrunfw is also bumping, edit `vendor/libkrun-checksums.txt` (the comment block documents the format) and the matching files at [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds): `version.txt` and `libkrunfw-version.txt`. Trigger the `build` workflow at libkrun-builds manually via `workflow_dispatch`.

**5. Update local checksums.** Once the libkrun-builds build publishes, copy each per-target SHA-256 from the release into `vendor/libkrun-checksums.txt`. The release packaging workflow refuses to use any downloaded bottle whose hash isn't listed there, so this is the supply-chain pin.

**6. Verify locally.** Run both build modes:

```bash
cargo build                          # stub path, must still compile
cargo check --features ward-core/krunvm   # real backend, downloads libkrun once
cargo test --workspace
```

**7. Commit as one PR.** Title: `chore(libkrun): bump to v<NEW>`. Include the diff summary in the PR body so reviewers can see what changed in the surface without re-running the script.

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

**Signed commits are required.** The `main` branch ruleset enforces `required_signatures`, so every commit must carry a valid GPG or SSH signature:

```bash
git commit --signoff --gpg-sign
```

If your local identity isn't set up for signing, see [git-scm.com/book/en/v2/Git-Tools-Signing-Your-Work](https://git-scm.com/book/en/v2/Git-Tools-Signing-Your-Work).

## Supply-chain posture

Ward maintains a tight CI security baseline; PRs that touch `.github/workflows/**` are checked automatically by the `workflow-hygiene` job:

- Every third-party GitHub Action is pinned to a commit SHA (mutable `@v<N>` tags rejected). Dependabot keeps the SHAs fresh; the `# v<N>` trailing comment lets diffs stay human-readable.
- Every workflow job runs under `step-security/harden-runner` with an egress allowlist. Build workflows (`ci.yml`, `release.yml`) enforce in `block` mode; API-only workflows allow `api.github.com` only.
- Release artefacts carry [SLSA build provenance](https://slsa.dev/spec/v1.0/provenance) via `actions/attest-build-provenance`. End users (and `install.sh`) verify with `gh attestation verify`.
- Branch ruleset for `main` is managed as code in [`igorjs/repo-config`](https://github.com/igorjs/repo-config) (see `repo-ward.tf`). Policy changes go through a PR there, not the GitHub UI.

The full posture matrix is in [SECURITY.md](SECURITY.md).

## Pull Request Process

1. Fork the repository and create a branch from `main`.
2. Make your changes with tests.
3. Ensure all checks pass: `cargo fmt --check && cargo clippy && cargo test`
4. Sign the CLA (first-time only).
5. Submit a PR with a clear description of what and why.
6. Address review feedback.

## Releasing

ward releases are cut by pushing a semver-tagged commit. CI produces pre-built archives for every supported target and publishes them to a GitHub Release; `install.sh` then resolves and downloads from that release.

### Cutting a release

```bash
# Bump the version in the root Cargo.toml [workspace.package].
git commit -am "chore: release v0.2.0"

# Push the tag. The `release` workflow takes over from here.
git tag -s v0.2.0 -m "ward 0.2.0"
git push origin v0.2.0
```

### What the release workflow produces

For every target in `{aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu}`:

- `ward-<version>-<target>.tar.gz` — contains `bin/ward`, `bin/wardd`, `bin/ward-mcp`, `LICENSE`, `README.md`, and `THIRD-PARTY-NOTICES.md`. libkrun, libkrunfw and the guest agent are embedded in the binaries.
- `ward-<version>-<target>.tar.gz.sha256` — checksum for `install.sh` verification.

### macOS codesign

The `aarch64-apple-darwin` build path ad-hoc signs `wardd` and `ward-mcp` with `entitlements.plist` (at workspace root) before staging. Two entitlements are needed. `com.apple.security.hypervisor` is required by the Hypervisor framework calls: without it, `krun_start_enter` returns `HV_ERROR_BAD_ARGUMENT` on Apple Silicon. `com.apple.security.cs.disable-library-validation` lets the hardened runtime load the libkrun that ward unpacks at first run.

`ward` (the CLI) is not signed because it never touches libkrun directly — it's a thin gRPC client.

Ad-hoc signing satisfies the entitlement requirement locally but does *not* pass Gatekeeper for distribution. End users may need `xattr -d com.apple.quarantine ~/.ward/bin/wardd` on first run. Tier-2 work (Developer ID + notarization) is tracked by [#30](https://github.com/igorjs/ward/issues/30).

For local builds from source (skipping the release pipeline), sign manually:

```sh
codesign --sign - \
  --entitlements entitlements.plist \
  --options runtime \
  --force \
  target/release/wardd target/release/ward-mcp
```

### What the release embeds

Every tagged release is built with the real microVM backend. `ward-bundle` downloads the pinned libkrun release, verifies it against `vendor/libkrun-checksums.txt` and embeds it with the guest agent. The matching SHA-256s must be committed in that file before a release can build.

### One-line install for users

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```

The script auto-detects target, downloads the latest tarball, verifies its SHA-256 against the published `.sha256` file, and installs binaries to `$HOME/.ward/bin/` (overridable via `WARD_INSTALL_DIR`).

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
