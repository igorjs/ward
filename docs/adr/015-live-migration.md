# ADR-015: Live Migration and Sandbox Hot-Swap

**Status:** Proposed (blocked on upstream libkrun API)
**Date:** 2026-05-27
**Authors:** Igor

## Context

Today ward's sandboxes are **ephemeral**:

- `ward create` boots a microVM. `wardd` shutdown loses every running
  sandbox.
- `ward snapshot create` ([ADR-009](009-snapshots.md)) captures
  **disk-only** state: the sandbox's rootfs as a tarball. Memory, CPU
  registers, open file descriptors, vsock state. None of these are
  serialised. Restore creates a new VM that boots from the captured
  rootfs.
- Cross-host migration is not supported (and isn't really possible
  without live state capture).

This is fine for the lead use case (ephemeral untrusted code execution
for AI agents, the workflow ward was designed around).
It is **not** fine for emerging use cases:

1. **Long-running sandboxes.** A workload that runs for hours or days
   inside a sandbox (e.g. a managed dev environment for a remote
   developer) shouldn't die because the operator restarted wardd for
   an unrelated reason.
2. **Cross-host migration.** Host running hot? Move sandboxes to a
   cooler host. Host needs rebooting? Drain to a buddy. Today: tell
   users they'll lose their work.
3. **Fork/warm-start.** Boot a "primer" sandbox to its first
   interactive state, snapshot the live memory + CPU, fork N sandboxes
   from that snapshot in <50ms each. Sub-100ms cold start.

All three require **live checkpoint/restore**: serialising the VM's
memory pages, vCPU registers, device state, open vsock connections,
etc.

## Status of upstream live snapshot

- **libkrun 1.18 does not expose live snapshot/restore in its public C
  API** as of the time of this ADR. Some support exists internally but
  is gated behind unreleased work.
- **Firecracker** has had live snapshot/restore (CRIU-style) since
  v0.23.0 (early 2020); it's a primary differentiator.
- **CRIU itself** could be used at the host level (ward checkpoints the
  libkrun process); fragile because libkrun + KVM/HVF state isn't
  CRIU-safe in general.

So implementing live migration in ward today means either:

1. **Wait for libkrun upstream**, then write a thin Rust wrapper.
2. **Add a Firecracker backend** alongside libkrun (per [ADR-012](012-backend-trait.md))
   and provide live migration only for Firecracker sandboxes.
3. **Custom CRIU integration**, with all its caveats. Not recommended.

## Decision

**Defer until libkrun's live snapshot API stabilises in a public
release, or until the Firecracker backend (option 2) is built.**

Either trigger unlocks the design below.

### Sandbox state envelope

A `Snapshot` becomes one of:

| Type | Contents | Created by |
|---|---|---|
| Disk-only (today) | rootfs.tar | `create_snapshot` |
| Live (future) | rootfs.tar + memory.bin + vcpu.json + vsock_state.json | `create_snapshot --live` |

The protobuf `Snapshot` message gains a `kind` enum field; existing
clients keep working (default = disk-only).

### Migration API

A new RPC `MigrateSandbox`:

```proto
service Ward {
    rpc MigrateSandbox(MigrateRequest) returns (MigrateResponse);
}

message MigrateRequest {
    string sandbox_id = 1;
    // Destination: either a remote wardd's gRPC URL, or "local"
    // to test the round-trip without actually moving the host.
    string target_url = 2;
    // Auth: bearer token for the destination wardd. Same scheme
    // as ADR-013's remote-access auth.
    string target_auth = 3;
}
```

Flow:

1. Source `wardd` pauses the sandbox (libkrun pause API).
2. Source captures the live snapshot.
3. Source streams the snapshot to the destination `wardd` over gRPC.
4. Destination restores the snapshot into a fresh microVM.
5. Source `wardd` removes the sandbox locally.
6. Destination's sandbox is unpaused and the client (CLI / SDK)
   updates its bookkeeping to talk to the destination.

Steps 1-2 take seconds for typical memory sizes (512 MiB - 4 GiB).
Steps 3-4 are network-bound. Step 6 is a client-side concern.

### Hot-swap (fork)

A new `ForkSandbox` RPC:

```proto
rpc ForkSandbox(ForkRequest) returns (ForkResponse);
message ForkRequest {
    string source_sandbox_id = 1;
    int32 count = 2;  // how many forks to spawn
}
```

The source sandbox is **paused, snapshotted, then forked**: N new
sandboxes resume from the snapshot's memory state. Useful for:

- Warm-starting test runners
- Multi-replica fuzzing
- Per-user dev environments cloned from a golden image at full
  runtime state (not just disk)

Sub-50ms per fork on libkrun once the underlying API exists.

## Why defer

- **The hard part is upstream libkrun work**, not ward's wrapper.
  Designing now risks committing to API shapes that don't match what
  libkrun publishes.
- **Memory snapshot files are large.** 512 MiB - 4 GiB per sandbox.
  Storage strategy (where they live, retention, deduplication via
  CoW filesystem) is non-trivial and depends on the workload pattern,
  which is currently unknown.
- **Cross-host migration adds a non-trivial security surface.** The
  snapshot stream contains the sandbox's full memory contents. mTLS
  or token auth on the destination is required (depends on
  [ADR-013](013-multi-tenant-auth.md) landing).

## Consequences

- Disk-only snapshots remain the only ward primitive for now. Documents
  this explicitly so users don't expect more.
- A pre-built deferred design lives here: when the upstream blocker
  clears, implementation starts from a known shape rather than
  greenfield.
- The Backend trait may need to grow `pause`, `resume`, `checkpoint`,
  `restore_live` methods. Existing backends (KrunvmBackend) return
  `Unimplemented` for the live ones until libkrun upstream catches up.
- [SECURITY.md](../../SECURITY.md) gains a row when live migration
  ships: "Live snapshot files contain VM memory; treat as encrypted
  at rest (host FDE) and encrypted in transit (mTLS / SSH tunnel)."

## Alternatives considered

- **Userspace VM serialisation via QEMU migration protocol.** Mature,
  well-tested, but pulls in QEMU which ward explicitly avoids
  (libkrun's whole point is "lightweight, no QEMU"). Wrong tradeoff.
- **Container-level CRIU at the host level.** Doesn't capture libkrun's
  internal state; loses sandbox isolation guarantees. Wrong layer.
- **"Just snapshot the OCI image."** That's [ADR-009](009-snapshots.md),
  already done. Doesn't capture memory/CPU state; not the same feature.
