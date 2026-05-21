# ADR-003: Isolation Backend

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs to run arbitrary code in isolated environments across macOS and Linux with hardware-backed kernel isolation stronger than Docker's namespace approach.

### Technologies evaluated

**Docker (runc):** Namespace and cgroup isolation. Shares the host kernel. Weakest boundary.

**gVisor (runsc):** Userspace kernel intercepting syscalls. Software-only. Linux only.

**Firecracker:** Full microVM with dedicated kernel. Linux only (KVM). Complex operational model.

**libkrun:** MicroVM library from the containers community (Red Hat). Per-sandbox microVMs using Apple Hypervisor.framework (HVF) on macOS 12+ ARM64 and KVM on Linux. Sub-second boot. Written in Rust. Apache 2.0. Published as `krun-sys` on crates.io.

## Decision

Ward uses **libkrun** via the `krun-sys` crate, wrapped in a safe Rust abstraction. The integration is gated behind the `krunvm` cargo feature so the daemon can be built without libkrun for development, testing, and packaging dry-runs.

### Why libkrun

1. **Hardware kernel isolation everywhere.** Each sandbox gets its own Linux kernel in its own microVM. HVF on macOS arm64, KVM on Linux.
2. **Same language ecosystem.** libkrun is Rust internally. `krun-sys` provides the bindings.
3. **Library, not daemon.** No "install krunvm first" step. libkrun links into wardd.
4. **Direct VM lifecycle control.** Function calls, not subprocess spawns.

### Integration architecture

```
ward-core
  └── backend/
        ├── mod.rs          Backend trait (see ADR-012), BackendError, ProcessHandle
        ├── krunvm.rs       Safe MicroVM wrapper over krun-sys
        └── image.rs        OCI image pull, unpack, and cache
```

All `unsafe` calls to `krun-sys` are confined to `krunvm.rs`. The rest of ward-core interacts only with the safe `Backend` trait.

### Feature flag

```toml
# ward-core/Cargo.toml
[features]
default = []
krunvm = ["dep:krun-sys"]

[dependencies]
krun-sys = { version = "1.10", optional = true }
```

**Default builds** (`cargo build`) compile without libkrun. The stub `KrunvmBackend` implementation returns synthetic UUIDs and scripted output streams — enough for the full test surface to exercise validation, manager logic, broker routing, and CLI dispatch.

**Real microVM builds** (`cargo build --features krunvm`) link libkrun. The same `KrunvmBackend` type performs real FFI calls instead of stubs.

This separation matters because:
- Tests run without libkrun installed
- Default CI is fast and platform-independent
- Developers on platforms where libkrun isn't available (Intel Mac, Windows + WSL2 quirks) can still build and run unit tests
- The eventual swap to a real backend changes one `--features` flag, not a code path

### OCI image handling

libkrun takes a local filesystem path as the root; it does not pull images. Ward handles image management separately in `ward-core/src/backend/image.rs`:

1. Pull OCI images (currently a stub; real implementation tracked as a follow-up).
2. Unpack image layers into `$WARD_DATA_DIR/images/<uuid>/rootfs`.
3. Pass the unpacked directory to libkrun via `krun_set_root`.

UUID-based directory names prevent path traversal attacks via maliciously crafted image references.

### Linking

`krun-sys`'s own `build.rs` uses pkg-config to find libkrun headers and library at build time. For developer builds, libkrun + libkrunfw must be installed via the system package manager (see CONTRIBUTING.md). For release artefacts, the libkrun dylibs are bundled alongside the binary via rpath; bottle production lives in the separate [`igorjs/ward-vendor`](https://github.com/igorjs/ward-vendor) repo.

### Isolation properties

| Property | Ward + libkrun |
|----------|---------------|
| Kernel isolation | Yes (separate Linux kernel per sandbox) |
| Hardware virtualization | Yes (HVF on macOS arm64, KVM on Linux) |
| Egress control | Yes (per-sandbox, see ADR-008) |
| Resource limits | Yes (vCPU and memory caps per microVM) |
| Boot time | Sub-second |
| OCI compatibility | Yes (Ward handles image pull and unpack) |
| Distribution | Single binary with bundled libkrun dylibs (release artefacts) |

## Consequences

- `unsafe` code in Ward is limited to the `krunvm.rs` wrapper calling through `krun-sys`. The `krun-sys` crate itself contains the raw FFI declarations.
- The feature flag means default builds work everywhere (the stub backend exercises the full code path); real microVM execution requires `--features krunvm` and libkrun installed.
- OCI image management is Ward's responsibility, not libkrun's.
- If upstream publishes a safe Rust API for libkrun, Ward's wrapper layer becomes thinner.
