# ADR-007: Platform Support and Hardware Requirements

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward uses libkrun for hardware-backed microVM isolation. libkrun requires Apple Hypervisor.framework (HVF) on macOS arm64 and KVM on Linux.

## Decision

### Supported platforms

| Platform | Architecture | Virtualization | Status |
|----------|-------------|----------------|--------|
| macOS 12+ (Monterey and later) | Apple Silicon (arm64) | Hypervisor.framework | First-class |
| Linux (kernel 5.10+) | amd64 | KVM | First-class |
| Linux (kernel 5.10+) | arm64 (Graviton) | KVM | First-class |

### Not supported

| Platform | Reason |
|----------|--------|
| macOS on Intel | Limited HVF support in libkrun for x86_64 Macs. Shrinking hardware base. |
| Windows (native) | No KVM, no HVF. No viable microVM path. |
| Windows (WSL2) | Ward's Linux binary works inside WSL2. Community-supported, not first-class. |

### Build modes

The daemon can be built in two modes (see ADR-003):

| Mode | Command | Backend | Use case |
|------|---------|---------|----------|
| Default | `cargo build` | Stub | Tests, CI, dev on unsupported platforms |
| Real microVM | `cargo build --features krunvm` | libkrun | Production, real isolation |

The default mode is what tests + CI run. It exercises every code path except actual VM boot.

### Prerequisites for real microVM mode

**macOS:**
- macOS 12 (Monterey) or later
- Apple Silicon (M1+)
- Developer install: `brew install slp/krun/libkrun slp/krun/libkrunfw` (see CONTRIBUTING.md)
- End-user install: libkrun dylibs bundled in release artefacts (built by [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds))

**Linux:**
- Kernel 5.10+ with KVM enabled (`/dev/kvm` accessible)
- Developer install: `apt install libkrun-dev libkrunfw-dev` (Debian/Ubuntu) or equivalent
- End-user install: libkrun shared objects bundled in release artefacts

### Distribution

1. **Pre-built binaries** via GitHub Releases for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
2. **One-line installer** via `curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash`
3. **Homebrew tap** for macOS: `brew install igorjs/ward/ward` (planned)
4. **apt repo** for Linux: `apt install ward` (planned)

Release artefacts bundle libkrun dylibs alongside the binary via rpath, so end users install nothing else.

### No fallback to weak isolation

If the platform does not support hardware virtualization and the build was made with `--features krunvm`, Ward fails with a clear error. There is no Docker/runc fallback. Stub-mode builds run on any platform but cannot boot real VMs.

## Consequences

- Both macOS-arm64 and Linux are first-class targets at launch.
- Zero runtime dependencies for end users — libkrun ships inside the release artefact.
- macOS 12+ covers nearly all Apple Silicon Macs in active use.
- Linux KVM is available on all major cloud providers and most bare metal servers.
- The feature flag means contributors can develop without installing libkrun until they specifically need to test microVM boot.
