# ADR-010: Shared Volumes

**Status:** Accepted
**Date:** 2026-05-03
**Authors:** Igor

## Context

Ward's mount system supports host-to-sandbox filesystem mounts: a directory on the host is bind-mounted into the sandbox. This covers the common case of mounting source code into a sandbox for execution.

However, pipeline workflows require data sharing between sandboxes without routing through the host filesystem:

- **Build pipelines:** Sandbox A compiles artefacts, Sandbox B runs integration tests against those artefacts.
- **Data processing:** Sandbox A generates a dataset, Sandbox B analyses it, Sandbox C visualises the results.
- **Agent collaboration:** Multiple AI agents work on different parts of a problem and share intermediate results.

Host mounts could technically solve this (both sandboxes mount the same host directory), but this breaks the isolation model: the host directory is writable by the host user, bypassing sandbox egress and permission controls. Shared volumes provide a daemon-managed alternative where the data lives inside Ward's isolation boundary.

## Decision

Ward supports shared volumes as daemon-managed storage that can be mounted into multiple sandboxes simultaneously. Volumes are independent of any single sandbox's lifecycle.

### RPC surface

```protobuf
service Ward {
  rpc CreateVolume  (CreateVolumeRequest)  returns (VolumeInfo);
  rpc GetVolume     (GetVolumeRequest)     returns (VolumeInfo);
  rpc ListVolumes   (google.protobuf.Empty) returns (ListVolumesResponse);
  rpc RemoveVolume  (RemoveVolumeRequest)  returns (google.protobuf.Empty);
}
```

Volumes are attached to sandboxes at creation time via the `volume_ids` field on `CreateSandboxRequest`:

```protobuf
CreateSandboxRequest {
  string image = 1;
  repeated Mount mounts = 2;
  repeated string volume_ids = 3;
  // ...
}
```

### Isolation guarantees

- Volumes are stored on the host under `$WARD_DATA_DIR/volumes/` and managed by the daemon.
- A volume mounted read-write in multiple sandboxes provides no concurrency control. Sandboxes writing to the same file simultaneously will race. This is documented as the user's responsibility, consistent with how Docker volumes behave.
- A volume mounted read-only in a sandbox cannot be written to by that sandbox, even if another sandbox has it mounted read-write.
- Volumes do not bypass egress controls. Network isolation is orthogonal to filesystem sharing.
- Deleting a volume that is currently mounted by any sandbox returns a NotFound error from the manager (volume bookkeeping prevents orphan mounts).

### Volume lifecycle

Volumes are explicitly created and explicitly deleted. They are not tied to any sandbox's lifecycle. Removing a sandbox that has a volume mounted does not remove the volume.

This is intentional: volumes may contain data that outlives individual sandbox runs. Cleanup is the user's responsibility via the API or CLI (`ward volume remove`).

### Capacity caps

The daemon enforces a configurable maximum number of volumes via `WARD_MAX_VOLUMES` (default: 256). Creating a volume above the cap returns `InvalidArgument` with a "limit reached" message. Removing a volume frees a slot.

### Backend implementation

Volumes are host directories mounted into the VM via libkrun's `krun_set_mapped_volumes` (when the krunvm feature is enabled). For development without libkrun, the stub backend returns synthetic IDs and tracks state in memory.

### Size limits

The proto schema includes a `size_mb` field. Currently unenforced — once libkrun supports per-volume size capping (e.g. via a loop-mounted ext4 image), the daemon will respect it. Until then, volumes are plain directories without size enforcement.

## Consequences

- Volumes enable pipeline and multi-agent workflows without breaking sandbox isolation.
- The daemon tracks volume-to-sandbox relationships to enforce deletion safety and report mount status.
- SDKs gain a `Volumes` resource on the Ward client (`ward.volumes.create()`, `ward.volumes.list()`, `ward.volumes.remove()`).
- Volume data persists on the host until explicitly deleted. Users must manage cleanup, especially in CI environments where volumes could accumulate.
- No distributed volume support in v1. Volumes are local to a single host. Multi-node volume sharing would require a networked filesystem (NFS, EFS) and is the remote management's concern.
