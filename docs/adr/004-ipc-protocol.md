# ADR-004: IPC Protocol

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs a protocol for communication between SDKs and the daemon, and between the remote management and daemon hosts. The protocol must support request/response patterns (create sandbox, get status) and server-streaming patterns (stdout/stderr output, subscribe to a pub/sub topic).

### Evaluated

**JSON over HTTP + SSE:** Simple, curl-debuggable. But requires hand-writing serialization in every SDK language. No schema enforcement. SSE is a workaround for streaming, not a first-class primitive.

**gRPC + protobuf:** Schema-driven. Code generation for every SDK language. Native bidirectional streaming. Binary wire format. Works over Unix socket (local) and TCP (remote). Single `.proto` file defines the entire API.

## Decision

Ward uses **gRPC + protobuf** for all communication. The single source of truth is `proto/ward.proto` in the repo. The protobuf package is `ward.v1`.

### Why gRPC everywhere

1. **One `.proto` file, all SDKs.** `ward.proto` defines every type and RPC. `protoc` generates typed clients for TypeScript, Python, Go, Rust, Ruby, Java, and more.
2. **Schema evolution.** Protobuf's field numbering guarantees backwards compatibility.
3. **Native streaming.** `StreamOutput` and `Subscribe` return `stream …` types. No SSE, no WebSocket, no polling.
4. **Same protocol at every boundary.** SDK to daemon (Unix socket), remote management to daemon (TCP + mTLS), SDK to remote management: all gRPC, all the same `.proto`.
5. **Debuggable.** `grpcurl` provides the same debugging experience as curl.

### Transport

**Local (SDK to daemon):** gRPC over Unix domain socket. Socket location follows platform conventions:
- macOS: `$HOME/.ward/ward.sock`
- Linux: `$XDG_RUNTIME_DIR/ward/ward.sock`, fallback `/tmp/ward-$USER/ward.sock`

Overridable via `WARD_SOCKET` env var. Socket permissions 0600 (owner only).

**Remote (remote management to daemon, SDK to remote management):** gRPC over TCP with mTLS or API key in metadata. Same `.proto`, same RPCs.

### Service surface (canonical: `proto/ward.proto`)

Twenty-one RPCs across seven groups. Full message definitions live in the proto file; this list is a summary.

```protobuf
service Ward {
  // Sandbox lifecycle (4)
  rpc CreateSandbox  (CreateSandboxRequest)  returns (SandboxInfo);
  rpc GetSandbox     (GetSandboxRequest)     returns (SandboxInfo);
  rpc ListSandboxes  (google.protobuf.Empty) returns (ListSandboxesResponse);
  rpc RemoveSandbox  (RemoveSandboxRequest)  returns (google.protobuf.Empty);

  // Process execution (5)
  rpc Exec           (ExecRequest)           returns (ProcessInfo);
  rpc Run            (RunRequest)            returns (ProcessInfo);
  rpc StreamOutput   (StreamOutputRequest)   returns (stream StreamEvent);
  rpc WriteStdin     (WriteStdinRequest)     returns (google.protobuf.Empty);
  rpc KillProcess    (KillProcessRequest)    returns (google.protobuf.Empty);

  // Snapshots (3)
  rpc CreateSnapshot   (CreateSnapshotRequest)   returns (SnapshotInfo);
  rpc RestoreSnapshot  (RestoreSnapshotRequest)  returns (google.protobuf.Empty);
  rpc ListSnapshots    (ListSnapshotsRequest)    returns (ListSnapshotsResponse);

  // Volumes (4)
  rpc CreateVolume  (CreateVolumeRequest)  returns (VolumeInfo);
  rpc GetVolume     (GetVolumeRequest)     returns (VolumeInfo);
  rpc ListVolumes   (google.protobuf.Empty) returns (ListVolumesResponse);
  rpc RemoveVolume  (RemoveVolumeRequest)  returns (google.protobuf.Empty);

  // Egress audit (1)
  rpc GetEgressLog  (GetEgressLogRequest) returns (EgressLogResponse);

  // Cross-sandbox communication (3) — see ADR-011
  rpc Publish              (PublishRequest)              returns (google.protobuf.Empty);
  rpc Subscribe            (SubscribeRequest)            returns (stream Message);
  rpc GetCommunicationLog  (GetCommunicationLogRequest)  returns (CommunicationLogResponse);

  // Daemon health + info (2)
  rpc GetHealth  (google.protobuf.Empty) returns (HealthStatus);
  rpc GetInfo    (google.protobuf.Empty) returns (DaemonInfo);
}
```

### Versioning

The protobuf package is `ward.v1`. Breaking changes (removing fields, changing field types) create `ward.v2`. Non-breaking additions (new fields, new RPCs) stay in `ward.v1`. The daemon can serve multiple versions simultaneously.

### Authentication

**Local (Unix socket):** No authentication. Socket file permissions (0600) provide access control.

**Remote (TCP):** API key in gRPC metadata (`authorization: Bearer ward-key-xxx`). mTLS for daemon-to-remote management.

### Error handling

gRPC status codes map naturally:

| Condition | gRPC status |
|-----------|------------|
| Sandbox not found | `NOT_FOUND` |
| Snapshot not found | `NOT_FOUND` |
| Process not found | `NOT_FOUND` |
| Invalid request | `INVALID_ARGUMENT` |
| Sandbox in Deny comms mode trying to publish/subscribe | `INVALID_ARGUMENT` |
| Internal error | `INTERNAL` |
| Not implemented | `UNIMPLEMENTED` |

Cross-tenant lookups (e.g. asking for a pid that belongs to a different sandbox) return `NOT_FOUND` rather than `PERMISSION_DENIED` to avoid leaking existence across sandbox boundaries.

## Consequences

- The `.proto` file is the single source of truth for the API.
- The daemon uses `tonic` (Rust gRPC framework). tonic supports Unix socket listeners natively.
- Generated bindings are produced at build time by `tonic-build` in `ward-core/build.rs`.
- Binary protobuf on the wire is more efficient than JSON but not human-readable. `grpcurl` with `-format json` bridges this gap.
- The remote management can proxy gRPC calls directly to daemon hosts without protocol translation.
