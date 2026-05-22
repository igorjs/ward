# Design: Completing the Ward microVM Backend

**Date:** 2026-05-24
**Author:** Igor (with Claude)
**Status:** Approved — ready for implementation planning

## Goal

Implement every pending backend feature in Ward so the daemon performs real
microVM work instead of stub bookkeeping. The gRPC API surface
(`proto/ward.proto`) is already complete and all RPC handlers dispatch; the
gap is in the libkrun backend and the host-side services around it.

Delivered as **one PR per feature**, each independently mergeable and
CI-green.

## Constraint: the verification model

The development environment has **no Rust toolchain, no protoc, and no
libkrun** installed, and standard GitHub-hosted runners do **not** reliably
provide `/dev/kvm`. Therefore:

- **There is no local build/test loop.** Every change is verified by pushing
  the branch, opening the PR, and reading GitHub Actions results
  (`gh pr checks`). CI is the single source of truth.
- **Host-side features** (OCI pull, volume images, egress proxy, snapshot
  file ops) receive real test coverage in the existing CI jobs and are
  genuinely verified.
- **libkrun runtime paths** (VM boot, in-guest exec, TAP attach, mount
  attach, VM restart) are verified only to the **compile + link +
  non-boot-unit-test** tier by a new `krunvm` CI job. Booting a real microVM
  is deferred until a **self-hosted KVM runner** exists (TBD). Boot-level
  integration tests are written but **gated** behind an env switch
  (`WARD_KVM_TESTS=1`) so that runner can enable them later with no rework.

Honest ceiling: PRs 4–7 ship runtime code that no available CI can execute.
Each is labelled "compile-verified, boot-unverified pending KVM runner."

## Decisions (locked during brainstorming)

1. **Scope:** implement all pending features; verify in CI.
2. **CI depth:** `--features krunvm` build + link + unit on standard runners
   now; plan for a self-hosted KVM boot job later.
3. **Snapshots:** disk-level (rootfs archive + metadata), since libkrun 1.18
   exposes no checkpoint/restore API. No live memory/CPU state.
4. **Exec model:** a new `ward-guest-agent` crate (libkrun boots one process
   per VM with no re-exec call, so a guest-side agent is required).

## Architecture

### New component: `ward-guest-agent`

A small Rust binary, statically linked for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, baked into every sandbox rootfs and set as
libkrun's exec target (`krun_set_exec`). It runs as the VM entry process,
listens on a **vsock** port, and:

- accepts `Exec` / `Run` requests, spawns the process, streams
  `stdout`/`stderr`/`exit` back, accepts `stdin` writes, handles `kill`;
- speaks a **minimal length-prefixed protobuf** protocol defined in a new
  internal `proto/ward_agent.proto` (not part of the public API).

The daemon side (`ward-core/src/backend/krunvm.rs`) configures the vsock port
(`krun_add_vsock_port2`), connects to the agent socket, and maps the public
`Exec/Run/StreamOutput/WriteStdin/KillProcess` RPCs onto agent messages.
`Run()`'s file-write is the agent writing the snippet to a temp file before
exec'ing the interpreter.

Agent logic and the wire protocol are unit-testable on a Linux host without a
VM (CI-verifiable). The boot/connect wiring is gated.

## Per-feature design

### 1. OCI image pull — `ward-core/src/backend/image.rs`

Pull manifest + layers via a pure-Rust OCI client (`oci-client`), verify
layer digests, unpack gzipped tar layers into
`$WARD_DATA_DIR/images/<uuid>/rootfs`, applying tar whiteouts for deletions.
Return the real `sha256:` digest. UUID directory names prevent path-traversal
from crafted image references. Tested against fixture tar layers in CI (no
network dependency in the test).

### 2. Volume disk images — `ward-core/src/volume/manager.rs`

Allocate a fixed-size sparse file via `truncate`, format with `mkfs.ext4`,
track real size. Attached to VMs later via `krun_add_disk2`. `mkfs.ext4` is
Linux-only, so this is exercised in Linux CI; on non-Linux the path returns a
clear unsupported error.

### 3. Guest agent + real exec — new `ward-guest-agent` crate, `krunvm.rs`

As described in Architecture. Covers `Exec`, `Run`, `StreamOutput`,
`WriteStdin`, `KillProcess`. Replaces the scripted-stub stream in
`krunvm.rs`. Includes `proto/ward_agent.proto` and build wiring to compile
the agent for musl targets and stage it into rootfs builds.

### 4. Networking + egress enforcement — `egress/proxy.rs`, `krunvm.rs`

A host-side forward proxy (HTTP `CONNECT` + domain allowlist + audit log),
fully tested standalone with a real client. The VM is wired to route egress
through it via `krun_add_net_tap` (Linux). Proxy server, policy evaluation,
and `GetEgressLog` are verified in CI; TAP attach is gated.

### 5. Mounts & volume attach — `krunvm.rs`

Map `CreateSandboxRequest.mounts` via `krun_add_virtiofs3` (honouring
`readonly`) and attach `volume_ids` disks via `krun_add_disk2`. Path
validation and source/target mapping are tested; the attach calls are gated.

### 6. Disk-level snapshots — `krunvm.rs`, snapshot storage

- **Create:** quiesce the sandbox, archive the rootfs, and write
  `metadata.json` (env, mounts, egress policy, resource limits) under
  `$WARD_DATA_DIR/snapshots/<snapshot_id>/`. Report real `size_bytes`.
- **Restore:** swap the rootfs back and reboot the VM.
- **`from_snapshot`:** seed a new sandbox's rootfs from the archived snapshot.

No live memory/CPU state is captured (documented limitation, ADR-009 update).
Archive/metadata file operations are verified in CI; the VM reboot is gated.

## PR sequence

| PR | Feature | Verifiable now? |
|----|---------|-----------------|
| 1 | `krunvm` CI build+link+unit job (+ gated KVM harness scaffold) | Full — also reveals whether current `--features krunvm` compiles |
| 2 | OCI image pull | Full (host-side tests) |
| 3 | Volume disk images (ext4) | Full (Linux CI) |
| 4 | `ward-guest-agent` + real exec/run/stream/stdin/kill | Compile/link/agent-logic only; boot gated |
| 5 | Networking + egress enforcement | Proxy fully tested; TAP attach gated |
| 6 | Mounts & volume attach | Mapping tested; attach gated |
| 7 | Disk-level snapshots | File ops tested; VM reboot gated |

PR 1 is first because it establishes the verification gate every later
libkrun PR relies on, and it surfaces any existing `--features krunvm` build
breakage (CI has never built that configuration). Fixing such breakage is
part of PR 1.

Each PR updates the relevant ADRs: 003 (backend), 008 (egress), 009
(snapshots), 010 (volumes), plus a new ADR for the guest agent.

## Testing strategy

- **Default (stub) tests:** existing `ward-core` unit/integration and
  `ward-daemon` e2e jobs continue to pass on every PR.
- **Host-side feature tests:** new unit/integration tests run in the existing
  CI jobs (OCI unpack from fixtures, volume image creation on Linux, proxy
  allowlist enforcement, snapshot archive/restore file ops, guest-agent
  protocol round-trips).
- **`krunvm` build tier:** new CI job installs `libkrun-dev`/`libkrunfw-dev`,
  runs `cargo build`/`clippy`/`test --features krunvm`. Proves FFI
  signatures, linking, and non-boot logic.
- **Boot tier (gated):** integration tests behind `WARD_KVM_TESTS=1`, written
  but not run until a KVM-capable runner exists.

## Risks & limitations

- PRs 4–7 cannot be boot-verified in available CI; they are compile-verified
  only. This is explicit and accepted.
- Snapshots never capture live process/memory state with libkrun 1.18.
- Egress enforcement and mounts depend on Linux TAP/virtiofs; macOS runtime
  behaviour is best-effort and not CI-covered.
- Writing correct Rust without a local compiler raises the chance of trivial
  build errors caught only on CI round-trips; mitigated by careful review and
  small PRs.
