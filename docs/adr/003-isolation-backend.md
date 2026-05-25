# ADR-003: Isolation Backend

**Status:** Accepted (amended 2026-05-18, see Update section below)
**Date:** 2026-05-12
**Authors:** Igor

> **Reader note:** the original Decision section below refers to the `krun-sys` crate as the binding layer. That is **no longer accurate**: as of 2026-05-18 ward declares the libkrun ABI directly via hand-maintained FFI in `ward-core/src/backend/krun_ffi.rs`. The libkrun choice itself is unchanged. See the [Update](#update--2026-05-18) section for the full rationale.

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

1. Pull OCI images via the `oci-client` crate and unpack their layer tarballs (gzip + whiteout handling) into the rootfs — implemented in `backend/image.rs`.
2. Unpack image layers into `$WARD_DATA_DIR/images/<uuid>/rootfs`.
3. Pass the unpacked directory to libkrun via `krun_set_root`.

UUID-based directory names prevent path traversal attacks via maliciously crafted image references.

### Linking

`krun-sys`'s own `build.rs` uses pkg-config to find libkrun headers and library at build time. For developer builds, libkrun + libkrunfw must be installed via the system package manager (see CONTRIBUTING.md). For release artefacts, the libkrun dylibs are bundled alongside the binary via rpath; bottle production lives in the separate [`igorjs/libkrun-builds`](https://github.com/igorjs/libkrun-builds) repo.

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

- `unsafe` code in Ward is limited to the `krunvm.rs` wrapper. Raw FFI declarations live in `ward-core/src/backend/krun_ffi.rs` (see Update below).
- The feature flag means default builds work everywhere (the stub backend exercises the full code path); real microVM execution requires `--features krunvm` and libkrun installed.
- OCI image management is Ward's responsibility, not libkrun's.
- If upstream publishes a safe Rust API for libkrun, Ward's wrapper layer becomes thinner.

## Update — 2026-05-18

The original decision (use libkrun, gated behind `--features krunvm`) is unchanged. What changed is the binding layer.

**Before:** Ward depended on the `krun-sys` crate from crates.io, which uses `bindgen` + `pkg-config` to generate FFI declarations at build time.

**After:** Ward declares the libkrun C ABI directly via hand-maintained `unsafe extern "C"` blocks in `ward-core/src/backend/krun_ffi.rs`.

**Reasons:**

1. **Upstream stale.** `krun-sys` 1.10.1 (Feb 2025) is pinned to libkrun's 1.10 ABI. Libkrun reached 1.18.0 by May 2026 with ~30 new functions that `krun-sys` doesn't expose. Ward needs several of them (`krun_setuid`/`krun_setgid` for non-root exec, `krun_add_net_tap` for egress proxying, `virtio-console-multiport` for combined stdout/stderr).
2. **Removes the `libclang` build dependency.** `bindgen` requires `libclang` at build time, which is a non-trivial install on minimal CI runners and was the proximate cause of `--features krunvm` build failures in commits prior to 5218cb6.
3. **API is auditable.** libkrun's C ABI is 60 functions with primitive/string/array signatures (no nested structs, no callback types). Maintaining these by hand is ~270 lines of trivially diff-able Rust. The cost of `bindgen`'s automation isn't justified at this size.
4. **All 60 symbols are declared.** `krun-sys` 1.10.1 shipped 30 of them. Hand-rolling means full coverage in one pass plus first-class control over future bumps.

**Linking:** the symbols still resolve through the system's libkrun + libkrunfw shared libraries. `ward-core/build.rs` emits `cargo:rustc-link-lib=krun` + `cargo:rustc-link-lib=krunfw` when the `krunvm` feature is on (work previously done by `krun-sys`'s own build.rs).

**Maintenance:** on each libkrun bump, diff `containers/libkrun/include/libkrun.h` against `krun_ffi.rs` and translate any new signatures. Procedure tracked in `CONTRIBUTING.md` and issue #32.

Implementation: issue #31.
