# ADR-009: Snapshots and State Management

**Status:** Accepted (implementation: disk-level archive/restore; live memory/CPU state not captured)
**Date:** 2026-05-03
**Authors:** Igor

## Context

AI agent workflows frequently involve multi-step processes where an agent sets up an environment (installs dependencies, configures tools, generates scaffolding) and then needs to branch into multiple exploratory paths. Without snapshots, each branch must repeat the entire setup. CI pipelines have a similar pattern.

Competitors in the sandbox space treat snapshots as a core feature. The ability to save, restore, and fork sandbox state enables workflows like:

- **Branching exploration:** an agent tries approach A from a snapshot, rolls back, tries approach B
- **Reproducible evaluation:** run the same test against the same snapshot across different models or prompts
- **Fast setup skip:** snapshot after `npm install` completes, restore from snapshot for every subsequent run
- **Checkpointing:** periodic snapshots during long-running tasks as a recovery mechanism

## Decision

Ward supports snapshots as a first-class API primitive via three RPCs:

```protobuf
service Ward {
  rpc CreateSnapshot   (CreateSnapshotRequest)   returns (SnapshotInfo);
  rpc RestoreSnapshot  (RestoreSnapshotRequest)  returns (google.protobuf.Empty);
  rpc ListSnapshots    (ListSnapshotsRequest)    returns (ListSnapshotsResponse);
}
```

### Current implementation state

Snapshots are **disk-level**: `create_snapshot` archives the sandbox rootfs to a tar under `$WARD_DATA_DIR/snapshots/<id>/` with a `metadata.json` sidecar and reports the real archive size; `restore_snapshot` swaps the rootfs back to the archived contents (after the ownership check); `from_snapshot` on `CreateSandbox` seeds a new sandbox's rootfs from a snapshot; removing a sandbox reaps its archives. Metadata is also tracked in memory for list/restore lookups.

**libkrun 1.18 still does not expose live checkpoint/restore in its public C API**, so only filesystem state is captured — in-memory and CPU state are not. If upstream adds a live checkpoint API, it can layer onto this disk-level mechanism without changing the gRPC contract. Under `--features krunvm`, restore additionally reboots the VM into the restored rootfs (gated; the filesystem swap is the host-side, verified half).

### Lifecycle binding

Snapshots are bound to their parent sandbox's lifetime. Removing the parent sandbox reaps its snapshots. This matches the proto's design (`ListSnapshots` is keyed by `sandbox_id`) and means there are no dangling snapshot rows.

### Cross-sandbox isolation

Restoring a snapshot owned by sandbox A from the perspective of sandbox B returns `NotFound`. The cross-sandbox lookup does not leak the snapshot's existence to the wrong caller.

### Snapshot creation (future live-checkpoint enhancement)

The disk-level mechanism above is what ships today. If libkrun gains a live checkpoint API, creation would additionally:

1. All running processes inside the sandbox are paused.
2. The filesystem state is captured as a copy-on-write layer.
3. Sandbox metadata (environment variables, mounts, egress policy, resource limits) is serialised alongside the filesystem state.
4. The sandbox is resumed.

### Restoration (future live-checkpoint enhancement)

With a live checkpoint API, restoring on an existing sandbox would:

1. All running processes are stopped.
2. The filesystem is reverted to the snapshot state.
3. Sandbox metadata is restored.
4. The sandbox is restarted in the restored state.

Alternatively, a new sandbox can be created from a snapshot via the `from_snapshot` field on `CreateSandboxRequest`. This creates a new, independent sandbox with the snapshotted state as its starting point.

### Storage

Snapshots are stored under `$WARD_DATA_DIR/snapshots/<id>/` as a directory containing the rootfs archive (`rootfs.tar`) and a `metadata.json` file. Snapshots are not automatically cleaned up beyond their parent sandbox removal.

### Limitations

- Snapshots are local to the host. They cannot be transferred between machines in v1.
- Process state restoration depends on libkrun's eventual checkpoint API.
- Snapshots of sandboxes with active network connections will not restore those connections. The sandboxed process must handle reconnection.

## Consequences

- Snapshot/restore adds storage overhead proportional to the size of the sandbox filesystem and the frequency of snapshots.
- The API surface is in place and stable; only the backend implementation changes when libkrun gains the capability.
- SDKs can implement `snapshot()`, `restore()`, and `snapshots()` methods against the disk-level backend today; live memory/CPU state is the only thing not captured.
- The daemon tracks snapshot metadata and garbage-collects snapshots when the parent sandbox is removed (no orphans possible).
