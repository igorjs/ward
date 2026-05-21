# ADR-009: Snapshots and State Management

**Status:** Accepted (implementation: metadata-only stub; real libkrun integration deferred)
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

The backend currently tracks snapshot **metadata** (snapshot_id, sandbox_id, label, timestamp) in an in-memory map. The full lifecycle works: create returns a UUID, restore validates the snapshot exists and belongs to the named sandbox, list scopes results to the requesting sandbox.

**libkrun 1.10 does not expose snapshot/restore in its public C API.** Once upstream adds it, the backend stub becomes a real checkpoint/restore call. The error semantics and gRPC contract stay unchanged.

### Lifecycle binding

Snapshots are bound to their parent sandbox's lifetime. Removing the parent sandbox reaps its snapshots. This matches the proto's design (`ListSnapshots` is keyed by `sandbox_id`) and means there are no dangling snapshot rows.

### Cross-sandbox isolation

Restoring a snapshot owned by sandbox A from the perspective of sandbox B returns `NotFound`. The cross-sandbox lookup does not leak the snapshot's existence to the wrong caller.

### Snapshot creation (intended)

When real libkrun snapshot support lands:

1. All running processes inside the sandbox are paused.
2. The filesystem state is captured as a copy-on-write layer.
3. Sandbox metadata (environment variables, mounts, egress policy, resource limits) is serialised alongside the filesystem state.
4. The sandbox is resumed.

### Restoration (intended)

Restoring on an existing sandbox:

1. All running processes are stopped.
2. The filesystem is reverted to the snapshot state.
3. Sandbox metadata is restored.
4. The sandbox is restarted in the restored state.

Alternatively, a new sandbox can be created from a snapshot via the `from_snapshot` field on `CreateSandboxRequest`. This creates a new, independent sandbox with the snapshotted state as its starting point.

### Storage

Real snapshots will be stored under `$WARD_DATA_DIR/snapshots/` as a directory containing the filesystem layer and a metadata JSON file. Snapshots are not automatically cleaned up beyond their parent sandbox removal.

### Limitations

- Snapshots are local to the host. They cannot be transferred between machines in v1.
- Process state restoration depends on libkrun's eventual checkpoint API.
- Snapshots of sandboxes with active network connections will not restore those connections. The sandboxed process must handle reconnection.

## Consequences

- Snapshot/restore adds storage overhead proportional to the size of the sandbox filesystem and the frequency of snapshots (once real implementation lands).
- The API surface is in place and stable; only the backend implementation changes when libkrun gains the capability.
- SDKs can implement `snapshot()`, `restore()`, and `snapshots()` methods today; they'll return metadata-only results until the real backend lands.
- The daemon tracks snapshot metadata and garbage-collects snapshots when the parent sandbox is removed (no orphans possible).
