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

Before making non-trivial changes, skim the ADRs under
[`docs/adr/`](docs/adr/). [`docs/SPEC.md`](docs/SPEC.md) is the table of
contents. Each ADR is short (~50–100 lines) and explains *why* a piece
of the system is shaped the way it is. Particularly useful starting
points:

- [ADR-001](docs/adr/001-project-scope.md) — what's in/out of scope
- [ADR-003](docs/adr/003-isolation-backend.md) — libkrun and the `krunvm` feature flag
- [ADR-004](docs/adr/004-ipc-protocol.md) — gRPC + proto schema source of truth
- [ADR-011](docs/adr/011-cross-sandbox-comms.md) — the pub/sub broker
- [ADR-012](docs/adr/012-backend-trait.md) — the `Backend` trait abstraction

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
inside the binary's rpath — bottle production lives in the separate
[`igorjs/ward-vendor`](https://github.com/igorjs/ward-vendor) repo —
but developer builds rely on the system package manager:

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

Why this isn't bundled at `cargo build` time: the "users install
nothing but ward" promise is satisfied by *end-user release artefacts*
(`.pkg`, `.deb`, install.sh) that bundle the dylibs, not by per-build
downloads. Bottles are produced by [`igorjs/ward-vendor`](https://github.com/igorjs/ward-vendor)
and consumed by `release.yml` in this repo. FFI declarations live in
`ward-core/src/backend/krun_ffi.rs` (hand-maintained, no `krun-sys`
crate, no bindgen, no libclang build-dep).

### Bumping libkrun

Ward pins a specific libkrun version in `vendor/libkrun-version.txt`
and declares the C ABI by hand in `ward-core/src/backend/krun_ffi.rs`.
When upstream releases a new version that ward should adopt, follow
this procedure end-to-end. The same convention lives in
[ADR-003 Update section](docs/adr/003-isolation-backend.md#update--2026-05-18).

**1. Diff the header.** Run the helper script with the target version:

```bash
scripts/diff-libkrun.sh <new-version>
# e.g. scripts/diff-libkrun.sh 1.19.0
```

It fetches `include/libkrun.h` for the currently-pinned version (from
`vendor/libkrun-version.txt`) and the new version, and prints a unified
diff. To list only added function declarations:

```bash
scripts/diff-libkrun.sh 1.19.0 | grep -E '^\+(int32_t|uint32_t|void) krun_'
```

**2. Translate new signatures.** For each added declaration in the
diff, add a matching `unsafe extern "C"` line in
`ward-core/src/backend/krun_ffi.rs`. Group by the existing section
comments (Context lifecycle, VM config, Networking, GPU/display,
Audio, Resource limits, Exec config, Firmware/kernel, TEE, vsock,
Console/serial, Virt features, Logging, Shutdown signalling, Boot).
Use the smallest correct Rust types: `int32_t` -> `i32`, `uint8_t` ->
`u8`, `const char *` -> `*const c_char`, NUL-terminated `char **` ->
`*const *const c_char`. Watch for `uint8_t` masquerading as a count
field (silent truncation if Rust callers pass a larger type; surface
as an error per the `krun_set_vm_config` precedent).

**3. Remove deletions.** If any function disappeared upstream, remove
its declaration here. Check libkrun's `ABI_VERSION` constant in
upstream `Makefile` is unchanged across the bump; if it incremented,
the SO version changed and we need a coordinated release rather than a
drop-in bump.

**4. Bump the pin and trigger a rebuild.** Edit
`vendor/libkrun-version.txt` to the new version. If libkrunfw is also
bumping, edit `vendor/libkrun-checksums.txt` (the comment block
documents the format) and the matching files at
[`igorjs/ward-vendor`](https://github.com/igorjs/ward-vendor):
`version.txt` and `libkrunfw-version.txt`. Trigger the `build`
workflow at ward-vendor manually via `workflow_dispatch`.

**5. Update local checksums.** Once the ward-vendor build publishes,
copy each per-target SHA-256 from the release into
`vendor/libkrun-checksums.txt`. The release packaging workflow
refuses to use any downloaded bottle whose hash isn't listed there,
so this is the supply-chain pin.

**6. Verify locally.** Run both build modes:

```bash
cargo build                          # stub path, must still compile
cargo check --features krunvm        # FFI path, requires libkrun installed
cargo test --workspace
```

**7. Commit as one PR.** Title: `chore(libkrun): bump to v<NEW>`.
Include the diff summary in the PR body so reviewers can see what
changed in the surface without re-running the script.

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

## Releasing

ward releases are cut by pushing a semver-tagged commit. CI produces
pre-built archives for every supported target and publishes them to a
GitHub Release; `install.sh` then resolves and downloads from that release.

### Cutting a release

```bash
# Bump the version in the root Cargo.toml [workspace.package].
git commit -am "chore: release v0.2.0"

# Push the tag. The `release` workflow takes over from here.
git tag -s v0.2.0 -m "ward 0.2.0"
git push origin v0.2.0
```

### What the release workflow produces

For every target in `{aarch64-apple-darwin, x86_64-unknown-linux-gnu,
aarch64-unknown-linux-gnu}`:

- `ward-<version>-<target>.tar.gz` — contains `bin/ward`, `bin/wardd`,
  `LICENSE`, `README.md`, plus `lib/libkrun.<ext>` and `lib/libkrunfw.<ext>`
  if the build is configured to bundle them (see below).
- `ward-<version>-<target>.tar.gz.sha256` — checksum for `install.sh`
  verification.

### Bundling libkrun in the release

The release workflow has two modes:

- **Stub mode** (default for tag pushes): builds without the `krunvm`
  feature. Binaries ship without microVM support — useful for the very
  first release and for demoing the CLI surface without provisioning
  hypervisor entitlements.
- **Bundled mode** (`workflow_dispatch` with `include_libkrun=true`):
  builds with `--features ward-core/krunvm` and copies the matching
  `libkrun.dylib`/`libkrunfw.dylib` from the `igorjs/ward-vendor`
  GitHub Release into the archive next to the binaries. Requires the
  `build` workflow at `ward-vendor` to have run for the pinned version
  AND the resulting SHA-256s to be committed here in
  `vendor/libkrun-checksums.txt`.

### One-line install for users

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```

The script auto-detects target, downloads the latest tarball, verifies
its SHA-256 against the published `.sha256` file, and installs binaries
to `$HOME/.ward/bin/` (overridable via `WARD_INSTALL_DIR`).

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
