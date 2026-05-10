# Ward Daemon: Complete Implementation Specification

Everything needed to implement the Ward daemon: architecture decisions, protobuf schema, project structure, and complete Rust source skeleton.

## Table of Contents

- ADR-001: Project Scope
- ADR-002: Language Choice (Rust)
- ADR-003: Isolation Backend (libkrun via krun-sys)
- ADR-004: IPC Protocol (gRPC + protobuf)
- ADR-005: SDK Strategy (proto-generated + idiomatic wrappers)
- ADR-006: Licensing
- ADR-007: Platform Support
- ADR-008: Egress Control
- ADR-009: Snapshots
- ADR-010: Shared Volumes
- Protobuf Schema (ward.proto)
- Rust Project Structure and Source Code

---

-e 
---

# ADR-001: Project Scope, Purpose, and Layering

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

The container ecosystem is dominated by Docker, a service orchestration platform repurposed for ephemeral job execution. Docker's overhead is acceptable for long-running services but wasteful for short-lived isolated workloads (CI jobs, AI agent sessions, code execution sandboxes). Worse, Docker's namespace-based isolation shares the host kernel and is vulnerable to container escape exploits.

Emerging SaaS alternatives (E2B, Daytona, Vercel Sandbox) solve isolation but require cloud dependencies. Open-source agent orchestrators offer simple DX but rely on plain Docker with no egress controls, no resource enforcement, and no kernel isolation.

There is a gap for a tool that combines hardware-backed microVM isolation with a simple, local-first developer experience, and that is useful beyond AI agent orchestration.

## Decision

**Ward** is a general-purpose sandbox daemon that creates, manages, and destroys isolated execution environments with first-class egress control, resource limits, and mount management. Each sandbox runs in its own microVM with its own Linux kernel via libkrun. Ward knows nothing about AI, prompts, git worktrees, or any specific workflow. It runs things in isolation.

### Layer 1: Ward Daemon (this project)

A compiled Rust binary that runs as a daemon, exposes a Unix socket API, and manages sandbox lifecycle:

- MicroVM creation from OCI images (via krunvm/libkrun)
- Command execution with streaming stdout/stderr
- Code string execution with language runtime detection
- Egress filtering (domain-level allowlisting via embedded proxy)
- Resource limits (CPU, memory, PID count, timeout)
- Filesystem mounts (read-only and read-write)
- Snapshots (save/restore sandbox state)
- Shared volumes (daemon-managed, cross-sandbox storage)
- Cleanup on crash, timeout, or explicit teardown

### Layer 2: SDKs (separate packages)

Thin, typed clients in multiple languages that communicate with the daemon over Unix socket IPC. SDKs are transport layers only. Intelligence lives in the daemon.

### Layer 3: remote management (separate project, proprietary)

A fleet management layer for enterprise customers. Manages scheduling, scaling, authentication, and billing across multiple Ward daemon hosts. Supports self-managed (EC2 + libkrun) and serverless (Lambda + Fargate) backends. See remote management ADRs.

### Out of scope for Ward

- AI agent orchestration, CI job scheduling, container image building
- Multi-node distribution (remote management's responsibility)
- Weak isolation fallbacks (no Docker/runc mode)

## Consequences

- Ward is useful to any project needing isolated execution, not just AI tooling.
- A single isolation backend (krunvm) simplifies testing, maintenance, and mental model.
- The SDK surface is small and stable because the API is simple.
- Enterprise scale is the remote management's job, not the daemon's.

-e 
---

# ADR-002: Language Choice

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward is a long-running daemon that manages concurrent sandbox lifecycles: wrapping the krunvm CLI for microVM creation, streaming I/O, enforcing timeouts, proxying egress, and cleaning up on failure. The language must support this workload, produce a single distributable binary, and align with the ecosystem Ward integrates with.

### Candidates evaluated

**Go:** Purpose-built for networked services. Goroutines map naturally to concurrent sandbox management. Excellent stdlib for HTTP, JSON, and sockets. Single static binary. The original candidate, evaluated and ultimately passed over.

**Rust:** Compiler-enforced memory safety without garbage collection. Strong async runtime (tokio) for concurrent I/O. Same language as krunvm and libkrun (the VMM ecosystem Ward wraps). Single static binary via musl. Steeper learning curve but stronger long-term guarantees.

**C/C++:** Maximum control but manual memory management in a long-running daemon is a reliability risk. No standard HTTP/JSON libraries. Rejected.

**Swift:** Native access to Apple frameworks but server-side ecosystem is thin and the project needs cross-platform support (macOS + Linux). Rejected.

## Decision

Ward is written in Rust.

### Rationale

1. **Same ecosystem as the VMM.** krunvm and libkrun are Rust. If Ward ever needs to go deeper than CLI wrapping (call libkrun as a library, contribute upstream, fork for customization), Rust gives native access with zero FFI overhead.

2. **Safety for a security-critical daemon.** Ward manages isolation boundaries. A memory bug in the daemon could compromise the isolation model. Rust's borrow checker eliminates use-after-free, double-free, and data races at compile time. In a long-running daemon that manages hundreds of concurrent sandboxes, this matters.

3. **Error handling.** The daemon's job is propagating errors from sandbox operations (VM creation failures, exec timeouts, egress denials) to the SDK. Rust's `Result<T, E>` with `?` propagation is purpose-built for this. Every error path is explicit and compiler-checked.

4. **Async concurrency.** tokio provides lightweight tasks (similar to goroutines), channels, timers, and an async HTTP server (axum/hyper). The daemon's concurrency pattern (manage N independent sandbox lifecycles, each with I/O streams and timeouts) maps directly to tokio tasks.

5. **Single static binary.** `cargo build --release --target x86_64-unknown-linux-musl` produces a fully static binary with no runtime dependencies. Same for `aarch64-unknown-linux-musl`. On macOS, the binary links to system frameworks only.

6. **Build to last.** Rust's edition system allows language evolution without breaking existing code. Cargo's dependency management and semver enforcement provide stability. The language prioritizes correctness over convenience, which aligns with Ward's goals.

### What Rust costs

- Slower initial development vs Go (estimated 30-50% slower for the first month).
- Async Rust has friction (pinning, lifetimes across await points). The daemon's async surface is manageable: HTTP server, subprocess I/O, timers.
- Longer compile times (~30-60 seconds for a clean build). Incremental builds are fast.
- Smaller contributor pool than Go. Accepted tradeoff per project goals.

## Consequences

- The daemon is a Cargo workspace with three crates: `wardd` (daemon binary), `ward-cli` (CLI binary), `ward-core` (shared library).
- Cross-compilation targets: `aarch64-apple-darwin` (macOS), `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (Linux).
- Key dependencies: `tokio` (async runtime), `axum` (HTTP server), `serde`/`serde_json` (serialization), `tokio-process` (subprocess management).
- krunvm integration is via `tokio::process::Command`, wrapping the CLI. No CGO, no FFI, no unsafe blocks for the core integration.

-e 
---

# ADR-003: Isolation Backend

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs to run arbitrary code in isolated environments across macOS and Linux with hardware-backed kernel isolation stronger than Docker's namespace approach.

### Technologies evaluated

**Docker (runc):** Namespace and cgroup isolation. Shares the host kernel. Weakest boundary.

**gVisor (runsc):** Userspace kernel intercepting syscalls. Software-only. Linux only.

**Apple Containerization:** Per-container lightweight VMs. macOS 26+ only.

**Firecracker:** Full microVM with dedicated kernel. Linux only (KVM). Complex operational model.

**libkrun:** MicroVM library from the containers community (Red Hat). Per-sandbox microVMs using Apple Hypervisor.framework (HVF) on macOS 12+ and KVM on Linux. Sub-second boot. Written in Rust. Apache 2.0. Published as `krun-sys` on crates.io with official Rust bindings.

## Decision

Ward uses **libkrun** via the official `krun-sys` crate, wrapped in a safe Rust abstraction. Ward ships as a single binary with libkrun statically linked. No external runtime dependencies.

### Why libkrun

1. **Hardware kernel isolation everywhere.** Each sandbox gets its own Linux kernel in its own microVM. HVF on macOS, KVM on Linux.

2. **Same language ecosystem.** libkrun is Rust internally. The `krun-sys` crate provides official Rust bindings maintained by libkrun's author (Sergio Lopez). Ward depends on this crate directly.

3. **Single binary distribution.** libkrun is statically linked. No "install krunvm first" step. Users download Ward and it works.

4. **Direct VM lifecycle control.** VM creation, configuration, and execution are function calls through `krun-sys`, not subprocess spawns with stdout parsing.

### Integration architecture

Ward depends on `krun-sys` (the official Rust bindings crate) and wraps it in a safe abstraction:

```
ward-core
  └── backend/
        ├── mod.rs          Error types, public traits
        ├── krunvm.rs        Safe MicroVM wrapper over krun-sys
        └── image.rs         OCI image pull, unpack, and cache
```

The `krun-sys` crate provides raw `unsafe` bindings to libkrun's C API. Ward's `krunvm.rs` wraps these in a safe `MicroVM` struct with RAII cleanup:

```rust
use krun_sys;

pub struct MicroVM {
    ctx: u32,
}

impl MicroVM {
    pub fn new(cpus: u8, memory_mb: u32) -> Result<Self> {
        let ctx = unsafe { krun_sys::krun_create_ctx() };
        if ctx < 0 {
            return Err(BackendError::LibkrunError("failed to create context".into()));
        }
        let ctx = ctx as u32;
        unsafe {
            krun_sys::krun_set_vm_config(ctx, cpus, memory_mb);
        }
        Ok(Self { ctx })
    }

    pub fn set_root(&self, path: &str) -> Result<()> { ... }
    pub fn set_exec(&self, cmd: &str, args: &[&str]) -> Result<()> { ... }
    pub fn add_volume(&self, host: &str, guest: &str) -> Result<()> { ... }
    pub fn start(self) -> Result<i32> { ... }
}

impl Drop for MicroVM {
    fn drop(&mut self) {
        unsafe { krun_sys::krun_free_ctx(self.ctx); }
    }
}
```

The rest of ward-core only touches the safe `MicroVM` API. All `unsafe` is confined to `krunvm.rs`, delegated to `krun-sys`.

### OCI image handling

libkrun takes a local filesystem path as the root. It does not pull images. Ward handles image management separately:

1. Pull OCI images using `oci-distribution` crate or `skopeo`/`crane`.
2. Unpack image layers into `$WARD_DATA_DIR/images/`.
3. Pass the unpacked directory to libkrun via `krun_set_root`.

libkrun handles VM lifecycle. Ward handles image lifecycle.

### Linking

`krun-sys` handles linking to libkrun via its own `build.rs`. Ward's `Cargo.toml` depends on `krun-sys`; linking details are the crate's responsibility.

For static linking (preferred for single-binary distribution), the build environment must have `libkrun` and `libkrunfw` development libraries installed. CI uses a Dockerfile for reproducible builds.

### Future: upstream Rust API

The `krun-sys` crate wraps libkrun's C API. libkrun's internal Rust types are not publicly exposed. We intend to collaborate with upstream (option 3) to publish a safe Rust API crate. If that happens, Ward drops the safe wrapper and depends on the upstream Rust crate directly. The `MicroVM` abstraction we build now serves as the design target for that upstream API.

### Isolation properties

| Property | Ward + libkrun |
|----------|---------------|
| Kernel isolation | Yes (separate Linux kernel per sandbox) |
| Hardware virtualization | Yes (HVF on macOS, KVM on Linux) |
| Egress control | Yes (Ward's embedded proxy, per-sandbox) |
| Resource limits | Yes (vCPU and memory caps per microVM) |
| Boot time | Sub-second |
| OCI compatibility | Yes (Ward handles image pull and unpack) |
| Distribution | Single binary (libkrun statically linked) |

## Consequences

- Ward ships as a single binary with zero runtime dependencies.
- `unsafe` code in Ward is limited to the `krunvm.rs` safe wrapper calling through `krun-sys`. The `krun-sys` crate itself contains the raw FFI declarations.
- OCI image management (pulling, unpacking, caching) is Ward's responsibility.
- Build environment requires libkrun and libkrunfw development libraries. A Dockerfile ensures reproducible builds.
- If upstream publishes a safe Rust API, Ward's wrapper layer becomes unnecessary and can be removed.

-e 
---

# ADR-004: IPC Protocol

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs a protocol for communication between SDKs and the daemon, and between the remote management and daemon hosts. The protocol must support request/response patterns (create sandbox, get status) and server-streaming patterns (stdout/stderr output).

### Evaluated

**JSON over HTTP + SSE:** Simple, curl-debuggable. But requires hand-writing serialization in every SDK language. No schema enforcement. SSE is a workaround for streaming, not a first-class primitive.

**gRPC + protobuf:** Schema-driven. Code generation for every SDK language. Native bidirectional streaming. Binary wire format. Works over Unix socket (local) and TCP (remote). Single `.proto` file defines the entire API.

## Decision

Ward uses **gRPC + protobuf** for all communication.

### Why gRPC everywhere

1. **One `.proto` file, all SDKs.** `ward.proto` defines every type and RPC. `protoc` generates typed clients for TypeScript, Python, Go, Rust, Ruby, Java, and more. Each SDK becomes generated code + a thin idiomatic wrapper (~200 lines).

2. **Schema evolution.** Protobuf's field numbering guarantees backwards compatibility. Adding a field to `CreateSandboxRequest` does not break existing SDKs. This is enforced by the protocol, not by convention.

3. **Native streaming.** `StreamOutput` returns a `stream StreamEvent`. No SSE, no WebSocket, no polling. gRPC's HTTP/2 multiplexing handles concurrent streams efficiently.

4. **Same protocol at every boundary.** SDK to daemon (Unix socket), remote management to daemon (TCP + mTLS), SDK to remote management (TCP + auth): all gRPC, all the same `.proto`. No protocol translation.

5. **Debuggable.** `grpcurl` provides the same debugging experience as curl:
   ```bash
   grpcurl -unix /path/to/ward.sock ward.v1.Ward/GetHealth
   grpcurl -unix /path/to/ward.sock ward.v1.Ward/ListSandboxes
   ```

### Transport

**Local (SDK to daemon):** gRPC over Unix domain socket. Socket location follows the same convention as before:
```
macOS:  $HOME/.ward/ward.sock
Linux:  $XDG_RUNTIME_DIR/ward/ward.sock (fallback: /tmp/ward-$USER/ward.sock)
```
Overridable via `WARD_SOCKET` env var. Socket permissions 0600 (owner only).

**Remote (remote management to daemon, SDK to remote management):** gRPC over TCP with mTLS (daemon) or API key in metadata (remote management). Same `.proto`, same RPCs. The remote management acts as a gRPC proxy that routes to the correct daemon host.

### Service definition

```protobuf
service Ward {
  // Sandbox lifecycle
  rpc CreateSandbox(CreateSandboxRequest) returns (SandboxInfo);
  rpc GetSandbox(GetSandboxRequest) returns (SandboxInfo);
  rpc ListSandboxes(google.protobuf.Empty) returns (ListSandboxesResponse);
  rpc RemoveSandbox(RemoveSandboxRequest) returns (google.protobuf.Empty);

  // Execution
  rpc Exec(ExecRequest) returns (ProcessInfo);
  rpc Run(RunRequest) returns (ProcessInfo);
  rpc StreamOutput(StreamOutputRequest) returns (stream StreamEvent);
  rpc WriteStdin(WriteStdinRequest) returns (google.protobuf.Empty);
  rpc KillProcess(KillProcessRequest) returns (google.protobuf.Empty);

  // Snapshots
  rpc CreateSnapshot(CreateSnapshotRequest) returns (SnapshotInfo);
  rpc RestoreSnapshot(RestoreSnapshotRequest) returns (google.protobuf.Empty);
  rpc ListSnapshots(ListSnapshotsRequest) returns (ListSnapshotsResponse);

  // Volumes
  rpc CreateVolume(CreateVolumeRequest) returns (VolumeInfo);
  rpc GetVolume(GetVolumeRequest) returns (VolumeInfo);
  rpc ListVolumes(google.protobuf.Empty) returns (ListVolumesResponse);
  rpc RemoveVolume(RemoveVolumeRequest) returns (google.protobuf.Empty);

  // Egress and health
  rpc GetEgressLog(GetEgressLogRequest) returns (EgressLogResponse);
  rpc GetHealth(google.protobuf.Empty) returns (HealthStatus);
  rpc GetInfo(google.protobuf.Empty) returns (DaemonInfo);
}
```

The full message definitions are in `proto/ward.proto` (source of truth).

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
| Invalid request | `INVALID_ARGUMENT` |
| Sandbox already stopped | `FAILED_PRECONDITION` |
| Volume in use | `FAILED_PRECONDITION` |
| Internal error | `INTERNAL` |
| Not implemented | `UNIMPLEMENTED` |

Error details use gRPC's standard `google.rpc.Status` with detail messages.

## Consequences

- The `.proto` file is the single source of truth for the API. All SDKs, the daemon, and the remote management are generated from or validated against it.
- SDK creation effort drops dramatically. The generated gRPC client handles serialization, deserialization, streaming, and error propagation. The idiomatic wrapper is minimal.
- JSON debugging via curl is replaced by `grpcurl`. Slightly less convenient but equally capable.
- The daemon uses `tonic` (Rust gRPC framework) instead of `axum` (HTTP). tonic supports Unix socket listeners natively.
- Binary protobuf on the wire is more efficient than JSON but not human-readable. `grpcurl` with `-plaintext` or `-format json` bridges this gap.
- The remote management can proxy gRPC calls directly to daemon hosts without protocol translation. This simplifies the remote management architecture significantly.

-e 
---

# ADR-005: SDK Strategy

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs SDKs in multiple languages. With gRPC + protobuf as the protocol (ADR-004), SDK creation becomes primarily a code generation task rather than a hand-writing task.

## Decision

### SDK architecture: generated clients + idiomatic wrappers

Each SDK consists of:

1. **Generated gRPC client** from `ward.proto` using the language's standard protobuf/gRPC toolchain. This handles serialization, deserialization, streaming, connection management, and error propagation.

2. **Idiomatic wrapper** (~200-500 lines, hand-written) that makes the generated client feel native to the language. This maps gRPC patterns to language conventions: `async/await` in TypeScript/Python, channels in Go, `Result` in Rust, blocks in Ruby.

### SDK tiers

**Tier 1 (ship with daemon v1.0):**

| SDK | Package | gRPC toolchain |
|-----|---------|---------------|
| TypeScript/Deno | `@igorjs/ward` | `@grpc/grpc-js` + `ts-proto` |
| Node.js | `@igorjs/ward` | `@grpc/grpc-js` + `ts-proto` |
| Python | `ward-sdk` | `grpcio` + `grpcio-tools` |

**Tier 2 (fast follow):**

| SDK | Package | gRPC toolchain |
|-----|---------|---------------|
| Go | `github.com/igorjs/ward-sdk-go` | `google.golang.org/grpc` + `protoc-gen-go` |
| Rust | `ward-sdk` | `tonic` + `prost` |
| Ruby | `ward-sdk` | `grpc` gem |

**Tier 3 (later):**

| SDK | Package | gRPC toolchain |
|-----|---------|---------------|
| Java/Kotlin | `dev.ward:ward-sdk` | `io.grpc` + `protobuf-java` |

### Idiomatic wrapper examples

The generated gRPC client is correct but verbose. The wrapper makes it feel natural.

**TypeScript/Deno and Node.js:**

```typescript
import { Ward } from "@igorjs/ward";

const ward = new Ward(); // auto-discovers Unix socket

// Zero-config
const sandbox = await ward.create();

// Configured
const sandbox = await ward.create({
  image: "node:22-alpine",
  mounts: [{ source: "./src", target: "/work" }],
  egress: { mode: "ALLOWLIST", domains: ["registry.npmjs.org"] },
  resources: { cpus: 2, memoryMb: 4096, timeoutSeconds: 600 },
});

// Execute and stream
const proc = await sandbox.exec({ command: ["npm", "test"], workingDir: "/work" });
for await (const event of proc.stream()) {
  if (event.type === "STDOUT") console.log(event.line);
  if (event.type === "EXIT") console.log(`Exit code: ${event.exitCode}`);
}

// Run code directly
await sandbox.run({ language: "python", code: 'print("hello")' });

// Snapshot and restore
const snap = await sandbox.snapshot({ label: "after-setup" });
await sandbox.restore(snap.snapshotId);

await sandbox.remove();
```

**Python:**

```python
from ward_sdk import Ward

async with Ward() as ward:
    sandbox = await ward.create(
        image="node:22-alpine",
        egress={"mode": "ALLOWLIST", "domains": ["registry.npmjs.org"]},
    )

    proc = await sandbox.exec(command=["npm", "test"], working_dir="/work")
    async for event in proc.stream():
        if event.type == "STDOUT":
            print(event.line)

    await sandbox.remove()
```

**Go:**

```go
client, _ := ward.NewClient() // auto-discovers socket

sandbox, _ := client.Create(ctx, &ward.CreateSandboxRequest{
    Image: "node:22-alpine",
    Resources: &ward.ResourceLimits{Cpus: 2, MemoryMb: 4096},
})

proc, _ := sandbox.Exec(ctx, &ward.ExecRequest{
    Command: []string{"npm", "test"},
})

stream, _ := proc.StreamOutput(ctx)
for {
    event, err := stream.Recv()
    if err != nil { break }
    fmt.Println(event.Line)
}

sandbox.Remove(ctx)
```

**Rust:**

```rust
let client = ward::Client::connect_unix(socket_path).await?;

let sandbox = client.create_sandbox(CreateSandboxRequest {
    image: "node:22-alpine".into(),
    resources: Some(ResourceLimits { cpus: 2, memory_mb: 4096, ..Default::default() }),
    ..Default::default()
}).await?;

let proc = client.exec(ExecRequest {
    sandbox_id: sandbox.id.clone(),
    command: vec!["npm".into(), "test".into()],
    ..Default::default()
}).await?;

let mut stream = client.stream_output(StreamOutputRequest {
    sandbox_id: sandbox.id.clone(),
    pid: proc.pid.clone(),
}).await?.into_inner();

while let Some(event) = stream.message().await? {
    println!("{}", event.line);
}

client.remove_sandbox(RemoveSandboxRequest { id: sandbox.id }).await?;
```

### SDK repository structure

```
github.com/igorjs/ward           -- daemon + proto (Rust, AGPL v3)
github.com/igorjs/ward-sdk-ts    -- TypeScript/Deno + Node.js SDK (Apache 2.0)
github.com/igorjs/ward-sdk-py    -- Python SDK (Apache 2.0)
github.com/igorjs/ward-sdk-go    -- Go SDK (Apache 2.0)
github.com/igorjs/ward-sdk-rs    -- Rust SDK (Apache 2.0)
github.com/igorjs/ward-sdk-rb    -- Ruby SDK (Apache 2.0)
github.com/igorjs/ward-sdk-jvm   -- Java/Kotlin SDK (Apache 2.0)
```

Each SDK repo contains:

1. A copy of (or git submodule to) `ward.proto`
2. Generated gRPC client code (committed, not `.gitignore`d, so users don't need protoc)
3. The idiomatic wrapper
4. Tests

### Build pipeline

A CI workflow in the main `ward` repo generates client code for all SDK languages whenever `ward.proto` changes, and opens PRs against each SDK repo with the updated generated code.

### Protocol specification

The `.proto` file at `proto/ward.proto` is the source of truth. It is released under CC0 1.0 (public domain) so third parties can generate their own clients without any license obligations.

## Consequences

- SDK creation effort is ~200-500 lines of wrapper code per language, not ~1000 lines of hand-written serialization.
- Schema changes propagate automatically to all SDKs via generated code.
- Third parties can generate clients in any gRPC-supported language from the `.proto` file.
- The `.proto` file must be maintained carefully: field numbers cannot be reused, fields cannot change type.
- Generated code is verbose but correct. The idiomatic wrapper is where the DX investment goes.

-e 
---

# ADR-006: Licensing

**Status:** Accepted
**Date:** 2026-05-02
**Authors:** Igor

## Context

Ward is infrastructure software with two distinct components: a daemon that provides the isolation capability, and SDKs that let applications consume it. These components have different adoption dynamics and different risk profiles regarding competitive exploitation.

The daemon is where Ward's competitive value lives. A cloud provider or SaaS company could take the daemon, add proprietary features, and offer it as a managed service without contributing back. This has happened repeatedly in the open-source infrastructure space (ElasticSearch/AWS OpenSearch, MongoDB/DocumentDB, Terraform/OpenTofu).

The SDKs are consumption interfaces. Friction in SDK adoption directly reduces Ward's utility. Developers embedding a Ward SDK in their application should not face licensing concerns about their own code.

## Decision

### Ward Daemon: AGPL v3 (GNU Affero General Public License, Version 3)

The AGPL v3 requires that anyone who modifies the daemon and runs it as a network service must make their modified source code available. This protects against the "SaaS loophole" where a company takes open-source software, runs it as a hosted service, and never releases their changes.

Specifically:

- Anyone can use Ward internally without restriction.
- Anyone can modify Ward for their own use.
- If you distribute a modified Ward binary, you must release your modifications under AGPL v3.
- If you run a modified Ward as a network service (e.g., a hosted sandbox platform), you must release your modifications under AGPL v3.
- Running an unmodified Ward daemon in production (including as part of a commercial product) does not trigger any source disclosure obligation.

### Ward SDKs: Apache License 2.0

All SDKs are licensed under Apache 2.0. This is a permissive license that allows unrestricted commercial use with two important protections over MIT:

1. **Explicit patent grant.** Contributors grant users a royalty-free patent license covering any patents that would be infringed by the contribution. This protects SDK users from patent claims by contributors.

2. **Patent retaliation clause.** If a user sues any contributor for patent infringement related to the SDK, their license to the SDK is automatically terminated. This deters patent trolling.

SDK users can embed Ward SDKs in proprietary, closed-source, commercial software without any obligation to release their own source code.

### Boundary between AGPL and Apache 2.0

The SDKs communicate with the daemon over a Unix socket using HTTP and JSON. This is an arms-length network interface, not a library link. Applications using the SDK are not derivative works of the AGPL-licensed daemon. The AGPL obligation applies only to the daemon binary itself and any modifications to it.

This is the same boundary model used by:

- MongoDB (SSPL server, Apache 2.0 drivers)
- Grafana (AGPL server, Apache 2.0 client libraries)
- Mastodon (AGPL server, MIT client libraries)

### Protocol specification

The OpenAPI spec (`docs/openapi.yaml`) is released under Creative Commons CC0 1.0 (public domain dedication). Anyone can use the spec to build their own SDK or compatible daemon without any license obligations. This ensures the protocol itself is not encumbered.

## Consequences

- Cloud providers cannot take Ward, add proprietary features, and sell it as a closed service without releasing their changes.
- Developers can use Ward SDKs in any project (open source or proprietary) without licensing concerns.
- Third parties can build alternative SDKs or compatible tools from the OpenAPI spec without touching any AGPL or Apache 2.0 code.
- Enterprise legal teams evaluating Ward will see a clean separation: AGPL for the server component they run on their own infrastructure, Apache 2.0 for the client library they embed in their code. This is a well-understood pattern.
- The AGPL may deter some companies from contributing to the daemon. This is an accepted tradeoff. Companies that are unwilling to contribute under AGPL are typically the ones most likely to extract value without giving back.

-e 
---

# ADR-007: Platform Support and Hardware Requirements

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward uses libkrun (statically linked) for hardware-backed microVM isolation. libkrun requires Apple Hypervisor.framework (HVF) on macOS and KVM on Linux.

## Decision

### Supported platforms

| Platform | Architecture | Virtualization | Status |
|----------|-------------|----------------|--------|
| macOS 12+ (Monterey and later) | Apple Silicon (arm64) | Hypervisor.framework | Supported, v1.0 |
| Linux (kernel 5.10+) | amd64 | KVM | Supported, v1.0 |
| Linux (kernel 5.10+) | arm64 (Graviton) | KVM | Supported, v1.0 |

### Not supported

| Platform | Reason |
|----------|--------|
| macOS on Intel | Limited HVF support in libkrun. Shrinking hardware base. |
| Windows (native) | No KVM, no HVF. No viable microVM path. |
| Windows (WSL2) | Ward's Linux binary works inside WSL2. Community-supported, not first-class. |

### Prerequisites

**macOS:**
- macOS 12 (Monterey) or later
- Apple Silicon (M1+)
- No external dependencies (libkrun is statically linked)

**Linux:**
- Kernel 5.10+ with KVM enabled (`/dev/kvm` accessible)
- No external dependencies (libkrun is statically linked)

### Distribution

1. **Pre-built binaries** via GitHub Releases for `aarch64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
2. **Homebrew tap** for macOS: `brew install igorjs/tap/ward`
3. **Install script** for Linux: `curl -fsSL https://ward.dev/install.sh | sh`
4. **Cargo install:** `cargo install ward`

### No fallback to weak isolation

If the platform does not support hardware virtualization (no HVF, no KVM), Ward fails with a clear error. No Docker/runc fallback.

## Consequences

- Both macOS and Linux are first-class at launch.
- Zero runtime dependencies. Download the binary and run it.
- macOS 12+ covers nearly all Apple Silicon Macs in active use.
- Linux KVM is available on all major cloud providers and most bare metal servers.

-e 
---

# ADR-008: Egress Control and Network Isolation

**Status:** Accepted
**Date:** 2026-05-02
**Authors:** Igor

## Context

A sandbox without network controls is not a sandbox. If arbitrary code can reach the internet, it can exfiltrate data, download malware, call home to a C2 server, or abuse credentials found in the environment. Docker provides no egress filtering by default. Existing open-source agent orchestrators provide none at all.

Egress control is one of Ward's key differentiators. It must be simple to configure (a list of allowed domains) and enforced at the network level (not bypassable by the sandboxed process).

## Decision

### Default: deny all egress

By default, a Ward sandbox has no outbound network access. This is the safe default. The sandboxed process cannot reach the internet, the local network, or any other host.

### Allowlist mode

Users can specify a list of allowed domains in the sandbox configuration:

```json
{
  "egress": {
    "mode": "allowlist",
    "domains": [
      "registry.npmjs.org",
      "api.github.com",
      "*.githubusercontent.com"
    ]
  }
}
```

Wildcard prefixes (`*.example.com`) are supported for subdomains. Bare wildcards (`*`) are not allowed as they would negate the purpose of the allowlist.

### Enforcement mechanism

**macOS (Apple Containerization):**

Each container gets its own IP address via Apple's vmnet framework. Ward runs a lightweight forward proxy (a small Go HTTP CONNECT proxy embedded in the daemon) that the container's traffic routes through. The proxy resolves DNS, checks the target domain against the allowlist, and either forwards or rejects the connection. DNS resolution happens on the host side, preventing DNS-based bypasses.

The container's network configuration is set to route all traffic through the proxy. Direct IP connections that bypass DNS are blocked by default (the proxy only allows connections to IPs that resolved from allowed domains).

**Linux (containerd + gVisor):**

The container runs in a network namespace with a veth pair. nftables rules on the host restrict egress from the container's namespace to the proxy only. The same forward proxy approach applies.

### What the proxy handles

- HTTP CONNECT tunnelling for HTTPS traffic
- Plain HTTP forwarding
- Domain validation against the allowlist
- Connection logging (which domain, when, allowed/denied)
- Timeout enforcement per connection

### What the proxy does not handle

- Deep packet inspection (not a goal)
- Content filtering (not a goal)
- Bandwidth throttling (resource limits handle this at the cgroup level)
- Ingress (sandboxes are not reachable from outside)

### Open mode

For use cases where egress filtering is not needed (trusted code, internal tooling), users can set:

```json
{
  "egress": {
    "mode": "open"
  }
}
```

This disables the proxy and allows unrestricted outbound access. Ward logs a warning when a sandbox is created with open egress.

### Logging

All egress attempts (allowed and denied) are logged by the daemon and available via the API:

```
GET /v1/sandboxes/:id/egress-log
```

This provides an audit trail of every outbound connection a sandboxed process attempted.

## Consequences

- Default-deny means sandboxes are safe out of the box. Users must explicitly opt in to network access.
- The forward proxy adds latency to outbound connections (one extra hop). For the typical use case (npm install, pip install, git clone), this is negligible.
- The proxy is embedded in the daemon, not a separate process. This simplifies deployment and lifecycle management.
- Domain-level filtering (not IP-level) means the proxy must resolve DNS and maintain a mapping. This prevents the common bypass where a sandboxed process resolves a domain to an IP and then connects directly to the IP.
- The egress log provides visibility into what a sandboxed agent or CI job actually accessed, which is valuable for security auditing and debugging.

-e 
---

# ADR-009: Snapshots and State Management

**Status:** Accepted
**Date:** 2026-05-03
**Authors:** Igor

## Context

AI agent workflows frequently involve multi-step processes where an agent sets up an environment (installs dependencies, configures tools, generates scaffolding) and then needs to branch into multiple exploratory paths. Without snapshots, each branch must repeat the entire setup. CI pipelines have a similar pattern: a common build step followed by parallel test suites that each need the same base state.

Competitors in the sandbox space treat snapshots as a core feature. The ability to save, restore, and fork sandbox state enables workflows like:

- **Branching exploration:** an agent tries approach A from a snapshot, rolls back, tries approach B
- **Reproducible evaluation:** run the same test against the same snapshot across different models or prompts
- **Fast setup skip:** snapshot after `npm install` completes, restore from snapshot for every subsequent run instead of reinstalling
- **Checkpointing:** periodic snapshots during long-running tasks as a recovery mechanism

## Decision

Ward supports snapshots as a first-class API primitive. A snapshot captures the full state of a sandbox (filesystem, running processes are stopped, environment variables, resource configuration) and stores it locally on the host.

### API

```
POST   /v1/sandboxes/:id/snapshot    Create a snapshot
POST   /v1/sandboxes/:id/restore     Restore from a snapshot
GET    /v1/sandboxes/:id/snapshots   List snapshots for a sandbox
DELETE /v1/snapshots/:snap_id        Delete a snapshot
```

### Snapshot creation

When a snapshot is requested:

1. All running processes inside the sandbox are paused.
2. The filesystem state is captured as a copy-on-write layer (implementation varies by backend).
3. Sandbox metadata (environment variables, mounts, egress policy, resource limits) is serialised alongside the filesystem state.
4. The sandbox is resumed.

Snapshots are identified by a daemon-generated ID and an optional user-provided label.

### Restoration

Restoring a snapshot on an existing sandbox:

1. All running processes are stopped.
2. The filesystem is reverted to the snapshot state.
3. Sandbox metadata is restored.
4. The sandbox is restarted in the restored state.

Alternatively, a new sandbox can be created from a snapshot:

```json
// POST /v1/sandboxes
{
  "from_snapshot": "snap_j3k4l5m6"
}
```

This creates a new, independent sandbox with the snapshotted state as its starting point. The original sandbox and the new sandbox share no state after creation.

### Backend implementation

**macOS (Apple Containerization):** Apple's Virtualization.framework supports VM snapshot and restore. The `container` CLI exposes this capability. Ward calls the appropriate CLI commands to capture and restore VM state.

**Linux (containerd + gVisor):** containerd supports container checkpointing via CRIU (Checkpoint/Restore in Userspace). gVisor has experimental CRIU support. For filesystem-only snapshots (no process state), Ward can use overlayfs snapshot layers managed by containerd's snapshotter.

### Storage

Snapshots are stored on the local filesystem under `$WARD_DATA_DIR/snapshots/`. Each snapshot is a directory containing the filesystem layer and a metadata JSON file. Snapshots are not automatically cleaned up; the user manages retention via the API or CLI.

### Limitations

- Snapshots are local to the host. They cannot be transferred between machines in v1.
- Process state restoration (resuming a running process exactly where it left off) depends on backend support and may not be available on all platforms. Filesystem-only snapshots are always supported.
- Snapshots of sandboxes with active network connections will not restore those connections. The sandboxed process must handle reconnection.

## Consequences

- Snapshot/restore adds storage overhead proportional to the size of the sandbox filesystem and the frequency of snapshots.
- The API surface grows but remains consistent with the existing resource-oriented pattern.
- SDKs gain `snapshot()`, `restore()`, and `snapshots()` methods on the Sandbox object.
- The daemon must track snapshot metadata and garbage-collect orphaned snapshots when the parent sandbox is removed (configurable: keep or cascade delete).
- Snapshot from one backend cannot be restored on a different backend (a macOS snapshot cannot be restored on Linux).

-e 
---

# ADR-010: Shared Volumes

**Status:** Accepted
**Date:** 2026-05-03
**Authors:** Igor

## Context

Ward's mount system (ADR-004) supports host-to-sandbox filesystem mounts: a directory on the host is bind-mounted into the sandbox. This covers the common case of mounting source code into a sandbox for execution.

However, pipeline workflows require data sharing between sandboxes without routing through the host filesystem:

- **Build pipelines:** Sandbox A compiles artefacts, Sandbox B runs integration tests against those artefacts.
- **Data processing:** Sandbox A generates a dataset, Sandbox B analyses it, Sandbox C visualises the results.
- **Agent collaboration:** Multiple AI agents work on different parts of a problem and share intermediate results.

Host mounts could technically solve this (both sandboxes mount the same host directory), but this breaks the isolation model: the host directory is writable by the host user, bypassing sandbox egress and permission controls. Shared volumes provide a daemon-managed alternative where the data lives inside Ward's isolation boundary.

## Decision

Ward supports shared volumes as daemon-managed storage that can be mounted into multiple sandboxes simultaneously. Volumes are independent of any single sandbox's lifecycle.

### API

```
POST   /v1/volumes           Create a volume
GET    /v1/volumes           List all volumes
GET    /v1/volumes/:id       Get volume details (including which sandboxes have it mounted)
DELETE /v1/volumes/:id       Remove a volume (fails if currently mounted)
```

Volumes are attached to sandboxes at creation time via the `volumes` field:

```json
// POST /v1/sandboxes
{
  "image": "python:3.12-alpine",
  "volumes": [
    { "id": "vol_n7o8p9q0", "target": "/data", "readonly": false },
    { "id": "vol_r1s2t3u4", "target": "/models", "readonly": true }
  ]
}
```

### Isolation guarantees

- Volumes are stored on the host under `$WARD_DATA_DIR/volumes/` and managed by the daemon.
- A volume mounted read-write in multiple sandboxes provides no concurrency control. Sandboxes writing to the same file simultaneously will race. This is documented as the user's responsibility, consistent with how Docker volumes behave.
- A volume mounted read-only in a sandbox cannot be written to by that sandbox, even if another sandbox has it mounted read-write.
- Volumes do not bypass egress controls. Network isolation is orthogonal to filesystem sharing.
- Deleting a volume that is currently mounted by any sandbox returns a 409 Conflict error.

### Volume lifecycle

Volumes are explicitly created and explicitly deleted. They are not tied to any sandbox's lifecycle. Removing a sandbox that has a volume mounted does not remove the volume.

This is intentional: volumes may contain data that outlives individual sandbox runs. Cleanup is the user's responsibility via the API or CLI (`ward volume rm`).

### Backend implementation

**macOS (Apple Containerization):** Volumes are host directories mounted into the VM via virtio-fs or shared directory mechanisms provided by Virtualization.framework. The daemon creates the directory, sets permissions, and passes it to the `container` CLI as a mount.

**Linux (containerd + gVisor):** Volumes are host directories bind-mounted into the container namespace. containerd handles the mount propagation.

### Size limits

Volumes have an optional size limit specified at creation time:

```json
{
  "name": "pipeline-data",
  "size_mb": 1024
}
```

If specified, the daemon creates a fixed-size filesystem image (ext4 on a loop device or a tmpfs with size cap) rather than a plain directory. This prevents a runaway sandbox from filling the host disk via a shared volume.

If no size is specified, the volume is a plain directory with no size enforcement.

## Consequences

- Volumes enable pipeline and multi-agent workflows without breaking sandbox isolation.
- The daemon must track volume-to-sandbox relationships to enforce deletion safety and report mount status.
- SDKs gain a `Volumes` resource on the Ward client (`ward.volumes.create()`, `ward.volumes.list()`, `ward.volumes.remove()`).
- Volume data persists on the host until explicitly deleted. Users must manage cleanup, especially in CI environments where volumes could accumulate.
- No distributed volume support in v1. Volumes are local to a single host. Multi-node volume sharing would require a networked filesystem (NFS, EFS) and is out of scope.


---

# Protobuf Schema

The `.proto` file is the single source of truth for the API. All SDKs are generated from it. It is released under CC0 1.0 (public domain).

-e 
### `proto/ward.proto`

```protobuf
syntax = "proto3";

package ward.v1;

import "google/protobuf/empty.proto";
import "google/protobuf/timestamp.proto";

// ---------------------------------------------------------------------------
// Ward service
// ---------------------------------------------------------------------------

service Ward {
  // Sandbox lifecycle
  rpc CreateSandbox(CreateSandboxRequest) returns (SandboxInfo);
  rpc GetSandbox(GetSandboxRequest) returns (SandboxInfo);
  rpc ListSandboxes(google.protobuf.Empty) returns (ListSandboxesResponse);
  rpc RemoveSandbox(RemoveSandboxRequest) returns (google.protobuf.Empty);

  // Execution
  rpc Exec(ExecRequest) returns (ProcessInfo);
  rpc Run(RunRequest) returns (ProcessInfo);
  rpc StreamOutput(StreamOutputRequest) returns (stream StreamEvent);
  rpc WriteStdin(WriteStdinRequest) returns (google.protobuf.Empty);
  rpc KillProcess(KillProcessRequest) returns (google.protobuf.Empty);

  // Snapshots
  rpc CreateSnapshot(CreateSnapshotRequest) returns (SnapshotInfo);
  rpc RestoreSnapshot(RestoreSnapshotRequest) returns (google.protobuf.Empty);
  rpc ListSnapshots(ListSnapshotsRequest) returns (ListSnapshotsResponse);

  // Volumes
  rpc CreateVolume(CreateVolumeRequest) returns (VolumeInfo);
  rpc GetVolume(GetVolumeRequest) returns (VolumeInfo);
  rpc ListVolumes(google.protobuf.Empty) returns (ListVolumesResponse);
  rpc RemoveVolume(RemoveVolumeRequest) returns (google.protobuf.Empty);

  // Egress
  rpc GetEgressLog(GetEgressLogRequest) returns (EgressLogResponse);

  // Health
  rpc GetHealth(google.protobuf.Empty) returns (HealthStatus);
  rpc GetInfo(google.protobuf.Empty) returns (DaemonInfo);
}

// ---------------------------------------------------------------------------
// Sandbox messages
// ---------------------------------------------------------------------------

message CreateSandboxRequest {
  string image = 1;
  repeated Mount mounts = 2;
  repeated string volume_ids = 3;
  EgressPolicy egress = 4;
  ResourceLimits resources = 5;
  map<string, string> env = 6;
  string from_snapshot = 7;  // optional: create from snapshot ID
}

message GetSandboxRequest {
  string id = 1;
}

message RemoveSandboxRequest {
  string id = 1;
}

message ListSandboxesResponse {
  repeated SandboxInfo sandboxes = 1;
}

message SandboxInfo {
  string id = 1;
  SandboxStatus status = 2;
  string image = 3;
  google.protobuf.Timestamp created_at = 4;
  string ip_address = 5;
  ResourceLimits resources = 6;
  google.protobuf.Timestamp expires_at = 7;
}

enum SandboxStatus {
  SANDBOX_STATUS_UNSPECIFIED = 0;
  SANDBOX_STATUS_CREATING = 1;
  SANDBOX_STATUS_RUNNING = 2;
  SANDBOX_STATUS_STOPPED = 3;
  SANDBOX_STATUS_FAILED = 4;
}

message Mount {
  string source = 1;
  string target = 2;
  bool readonly = 3;
}

message EgressPolicy {
  EgressMode mode = 1;
  repeated string domains = 2;
}

enum EgressMode {
  EGRESS_MODE_UNSPECIFIED = 0;
  EGRESS_MODE_DENY = 1;
  EGRESS_MODE_ALLOWLIST = 2;
  EGRESS_MODE_OPEN = 3;
}

message ResourceLimits {
  uint32 cpus = 1;
  uint32 memory_mb = 2;
  uint32 pids_max = 3;
  uint64 timeout_seconds = 4;
}

// ---------------------------------------------------------------------------
// Execution messages
// ---------------------------------------------------------------------------

message ExecRequest {
  string sandbox_id = 1;
  repeated string command = 2;
  string working_dir = 3;
  map<string, string> env = 4;
}

message RunRequest {
  string sandbox_id = 1;
  string language = 2;
  string code = 3;
}

message ProcessInfo {
  string pid = 1;
  string sandbox_id = 2;
  string status = 3;
}

message StreamOutputRequest {
  string sandbox_id = 1;
  string pid = 2;
}

message StreamEvent {
  StreamEventType type = 1;
  string line = 2;
  int32 exit_code = 3;
  google.protobuf.Timestamp timestamp = 4;
  uint64 duration_ms = 5;
}

enum StreamEventType {
  STREAM_EVENT_TYPE_UNSPECIFIED = 0;
  STREAM_EVENT_TYPE_STDOUT = 1;
  STREAM_EVENT_TYPE_STDERR = 2;
  STREAM_EVENT_TYPE_EXIT = 3;
}

message WriteStdinRequest {
  string sandbox_id = 1;
  string pid = 2;
  bytes data = 3;
}

message KillProcessRequest {
  string sandbox_id = 1;
  string pid = 2;
}

// ---------------------------------------------------------------------------
// Snapshot messages
// ---------------------------------------------------------------------------

message CreateSnapshotRequest {
  string sandbox_id = 1;
  string label = 2;
}

message RestoreSnapshotRequest {
  string sandbox_id = 1;
  string snapshot_id = 2;
}

message ListSnapshotsRequest {
  string sandbox_id = 1;
}

message ListSnapshotsResponse {
  repeated SnapshotInfo snapshots = 1;
}

message SnapshotInfo {
  string snapshot_id = 1;
  string sandbox_id = 2;
  string label = 3;
  google.protobuf.Timestamp created_at = 4;
  uint64 size_bytes = 5;
}

// ---------------------------------------------------------------------------
// Volume messages
// ---------------------------------------------------------------------------

message CreateVolumeRequest {
  string name = 1;
  uint32 size_mb = 2;
}

message GetVolumeRequest {
  string id = 1;
}

message RemoveVolumeRequest {
  string id = 1;
}

message ListVolumesResponse {
  repeated VolumeInfo volumes = 1;
}

message VolumeInfo {
  string id = 1;
  string name = 2;
  uint32 size_mb = 3;
  google.protobuf.Timestamp created_at = 4;
  string mount_path = 5;
}

// ---------------------------------------------------------------------------
// Egress messages
// ---------------------------------------------------------------------------

message GetEgressLogRequest {
  string sandbox_id = 1;
}

message EgressLogResponse {
  repeated EgressLogEntry entries = 1;
}

message EgressLogEntry {
  string sandbox_id = 1;
  string domain = 2;
  string port = 3;
  bool allowed = 4;
  google.protobuf.Timestamp timestamp = 5;
}

// ---------------------------------------------------------------------------
// Health messages
// ---------------------------------------------------------------------------

message HealthStatus {
  string status = 1;
  uint64 uptime_seconds = 2;
  uint32 sandbox_count = 3;
  google.protobuf.Timestamp checked_at = 4;
}

message DaemonInfo {
  string version = 1;
  string platform = 2;
  string backend = 3;
  string arch = 4;
}
```

---

# Rust Project Structure and Source Code

## Directory Layout

```
ward/
  Cargo.toml              Workspace root
  proto/
    ward.proto            gRPC service + message definitions (source of truth)
  wardd/                  Daemon binary
    Cargo.toml
    src/main.rs
  ward-cli/               CLI binary
    Cargo.toml
    src/main.rs
  ward-core/              Shared library
    Cargo.toml
    build.rs              Compiles ward.proto + links libkrun
    src/
      lib.rs              Crate root, includes generated pb module
      config.rs
      protocol.rs         Internal types (with to_proto/from_proto conversions)
      grpc/
        mod.rs
        server.rs         tonic gRPC server implementing ward.v1.Ward
      backend/
        mod.rs            Error types
        krunvm.rs         Safe MicroVM wrapper over krun-sys
        image.rs          OCI image pull, unpack, and cache
      egress/
        mod.rs
        proxy.rs          Per-sandbox domain-level egress filtering
      sandbox/
        mod.rs
        manager.rs        Lifecycle coordinator (backend + egress + timeouts)
      volume/
        mod.rs
        manager.rs        Shared volume management
  docs/adr/               Architecture decision records
```

## Source Files

-e 
### `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "wardd",
    "ward-cli",
    "ward-core",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
authors = ["Igor <igor@igorjs.dev>"]
license = "AGPL-3.0-only"
repository = "https://github.com/igorjs/ward"
```
-e 
### `.gitignore`

```
/target
*.swp
*.swo
*~
.DS_Store
.idea/
.vscode/
.ward/
```
-e 
### `ward-core/Cargo.toml`

```toml
[package]
name = "ward-core"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[dependencies]
chrono = { version = "0.4", features = ["serde"] }
krun-sys = "0.1"
oci-distribution = "0.11"
prost = "0.13"
prost-types = "0.13"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tonic = "0.12"
tracing = "0.1"
uuid = { version = "1", features = ["v4"] }

[build-dependencies]
tonic-build = "0.12"
```
-e 
### `ward-core/build.rs`

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile ward.proto into Rust types and gRPC service traits
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward.proto"], &["../proto"])?;

    // Link libkrun
    println!("cargo:rustc-link-lib=krun");
    println!("cargo:rustc-link-lib=krunfw");

    Ok(())
}
```
-e 
### `ward-core/src/lib.rs`

```rust
pub mod backend;
pub mod config;
pub mod egress;
pub mod grpc;
pub mod protocol;
pub mod sandbox;
pub mod volume;

/// Generated protobuf types and gRPC service traits.
pub mod pb {
    tonic::include_proto!("ward.v1");
}
```
-e 
### `ward-core/src/config.rs`

```rust
//! Daemon configuration from environment variables with platform-appropriate defaults.

use std::env;
use std::path::PathBuf;

pub struct Config {
    pub socket_path: String,
    pub data_dir: String,
    pub log_level: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            socket_path: env::var("WARD_SOCKET")
                .unwrap_or_else(|_| default_socket_path()),
            data_dir: env::var("WARD_DATA_DIR")
                .unwrap_or_else(|_| default_data_dir()),
            log_level: env::var("WARD_LOG_LEVEL")
                .unwrap_or_else(|_| "info".into()),
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        let socket_parent = PathBuf::from(&self.socket_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();

        for dir in [
            socket_parent,
            PathBuf::from(&self.data_dir),
            PathBuf::from(&self.data_dir).join("snapshots"),
            PathBuf::from(&self.data_dir).join("volumes"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

fn default_socket_path() -> String {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        return format!("{}/ward/ward.sock", runtime_dir);
    }

    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());

    if cfg!(target_os = "macos") {
        format!("{}/.ward/ward.sock", home)
    } else {
        let user = env::var("USER").unwrap_or_else(|_| "unknown".into());
        format!("/tmp/ward-{}/ward.sock", user)
    }
}

fn default_data_dir() -> String {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{}/.ward/data", home)
}
```
-e 
### `ward-core/src/protocol.rs`

```rust
//! Shared types used across the Ward daemon.
//!
//! These types represent the API contract between the daemon's HTTP layer,
//! the krunvm backend, and the SDK clients. They serialize to the JSON
//! wire format that SDKs consume.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateOpts {
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub egress: EgressPolicy,
    #[serde(default)]
    pub resources: ResourceLimits,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub from_snapshot: Option<String>,
}

fn default_image() -> String {
    "alpine:latest".into()
}

impl CreateOpts {
    pub fn with_defaults(mut self) -> Self {
        if self.image.is_empty() {
            self.image = default_image();
        }
        if self.egress.mode == EgressMode::Unset {
            self.egress.mode = EgressMode::Deny;
        }
        self.resources = self.resources.with_defaults();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EgressPolicy {
    #[serde(default)]
    pub mode: EgressMode,
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    Deny,
    Allowlist,
    Open,
    #[default]
    Unset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default)]
    pub cpus: u32,
    #[serde(default)]
    pub memory_mb: u32,
    #[serde(default)]
    pub pids_max: u32,
    #[serde(default)]
    pub timeout_seconds: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpus: 0,
            memory_mb: 0,
            pids_max: 0,
            timeout_seconds: 0,
        }
    }
}

impl ResourceLimits {
    pub fn with_defaults(mut self) -> Self {
        if self.cpus == 0 {
            self.cpus = 2;
        }
        if self.memory_mb == 0 {
            self.memory_mb = 4096;
        }
        if self.pids_max == 0 {
            self.pids_max = 256;
        }
        if self.timeout_seconds == 0 {
            self.timeout_seconds = 1200;
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub id: String,
    pub status: SandboxStatus,
    pub image: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    pub resources: ResourceLimits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Creating,
    Running,
    Stopped,
    Failed,
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOpts {
    pub command: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOpts {
    pub language: String,
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: String,
    pub sandbox_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String, // "stdout", "stderr", "exit"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotOpts {
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub snapshot_id: String,
    pub sandbox_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Volumes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeCreateOpts {
    pub name: String,
    #[serde(default)]
    pub size_mb: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub mount_path: String,
}

// ---------------------------------------------------------------------------
// Language runtimes
// ---------------------------------------------------------------------------

pub struct LanguageRuntime {
    pub command: &'static str,
    pub extension: &'static str,
}

pub fn default_runtimes() -> std::collections::HashMap<&'static str, LanguageRuntime> {
    let mut m = std::collections::HashMap::new();
    m.insert("python", LanguageRuntime { command: "python3", extension: ".py" });
    m.insert("javascript", LanguageRuntime { command: "node", extension: ".js" });
    m.insert("typescript", LanguageRuntime { command: "npx tsx", extension: ".ts" });
    m.insert("ruby", LanguageRuntime { command: "ruby", extension: ".rb" });
    m.insert("go", LanguageRuntime { command: "go run", extension: ".go" });
    m.insert("bash", LanguageRuntime { command: "bash", extension: ".sh" });
    m.insert("sh", LanguageRuntime { command: "sh", extension: ".sh" });
    m
}

// ---------------------------------------------------------------------------
// API meta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub platform: String,
    pub backend: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub status: String,
    pub uptime_seconds: u64,
    pub sandbox_count: usize,
    pub checked_at: DateTime<Utc>,
}
```
-e 
### `ward-core/src/grpc/mod.rs`

```rust
mod server;
pub use server::WardGrpcServer;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```
-e 
### `ward-core/src/grpc/server.rs`

```rust
//! Ward gRPC server implementing the ward.v1.Ward service.
//!
//! This is the daemon's network-facing layer. It translates gRPC requests
//! into SandboxManager and VolumeManager calls.

use std::sync::Arc;
use std::time::Instant;

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::warn;

use crate::pb;
use crate::pb::ward_server::Ward;
use crate::sandbox::SandboxManager;
use crate::volume::VolumeManager;

pub struct WardGrpcServer {
    pub sandbox: Arc<SandboxManager>,
    pub volume: Arc<VolumeManager>,
    pub started_at: Instant,
}

#[tonic::async_trait]
impl Ward for WardGrpcServer {
    // -----------------------------------------------------------------------
    // Sandbox lifecycle
    // -----------------------------------------------------------------------

    async fn create_sandbox(
        &self,
        request: Request<pb::CreateSandboxRequest>,
    ) -> Result<Response<pb::SandboxInfo>, Status> {
        let req = request.into_inner();
        let opts = crate::protocol::CreateOpts::from_proto(req);

        match self.sandbox.create(opts).await {
            Ok(info) => Ok(Response::new(info.to_proto())),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn get_sandbox(
        &self,
        request: Request<pb::GetSandboxRequest>,
    ) -> Result<Response<pb::SandboxInfo>, Status> {
        let id = request.into_inner().id;
        match self.sandbox.get(&id).await {
            Ok(info) => Ok(Response::new(info.to_proto())),
            Err(_) => Err(Status::not_found(format!("sandbox {} not found", id))),
        }
    }

    async fn list_sandboxes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pb::ListSandboxesResponse>, Status> {
        let sandboxes = self.sandbox.list().await;
        Ok(Response::new(pb::ListSandboxesResponse {
            sandboxes: sandboxes.into_iter().map(|s| s.to_proto()).collect(),
        }))
    }

    async fn remove_sandbox(
        &self,
        request: Request<pb::RemoveSandboxRequest>,
    ) -> Result<Response<()>, Status> {
        let id = request.into_inner().id;
        match self.sandbox.remove(&id).await {
            Ok(()) => Ok(Response::new(())),
            Err(e) => Err(Status::not_found(e.to_string())),
        }
    }

    // -----------------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------------

    async fn exec(
        &self,
        request: Request<pb::ExecRequest>,
    ) -> Result<Response<pb::ProcessInfo>, Status> {
        let req = request.into_inner();
        let opts = crate::protocol::ExecOpts {
            command: req.command,
            working_dir: if req.working_dir.is_empty() {
                None
            } else {
                Some(req.working_dir)
            },
            env: req.env,
        };

        match self.sandbox.exec(&req.sandbox_id, opts).await {
            Ok(handle) => Ok(Response::new(pb::ProcessInfo {
                pid: handle.info.pid,
                sandbox_id: handle.info.sandbox_id,
                status: "running".into(),
            })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn run(
        &self,
        request: Request<pb::RunRequest>,
    ) -> Result<Response<pb::ProcessInfo>, Status> {
        let req = request.into_inner();
        let opts = crate::protocol::RunOpts {
            language: req.language,
            code: req.code,
        };

        match self.sandbox.run(&req.sandbox_id, opts).await {
            Ok(handle) => Ok(Response::new(pb::ProcessInfo {
                pid: handle.info.pid,
                sandbox_id: handle.info.sandbox_id,
                status: "running".into(),
            })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    type StreamOutputStream = ReceiverStream<Result<pb::StreamEvent, Status>>;

    async fn stream_output(
        &self,
        request: Request<pb::StreamOutputRequest>,
    ) -> Result<Response<Self::StreamOutputStream>, Status> {
        let _req = request.into_inner();

        // TODO: look up the process handle by sandbox_id + pid,
        // then forward events from ProcessHandle::event_rx to the gRPC stream.
        //
        // let (tx, rx) = tokio::sync::mpsc::channel(64);
        // tokio::spawn(async move {
        //     while let Some(event) = process_handle.event_rx.recv().await {
        //         let _ = tx.send(Ok(event.to_proto())).await;
        //     }
        // });
        // Ok(Response::new(ReceiverStream::new(rx)))

        Err(Status::unimplemented("streaming not yet implemented"))
    }

    async fn write_stdin(
        &self,
        _request: Request<pb::WriteStdinRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("stdin not yet implemented"))
    }

    async fn kill_process(
        &self,
        _request: Request<pb::KillProcessRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("kill not yet implemented"))
    }

    // -----------------------------------------------------------------------
    // Snapshots
    // -----------------------------------------------------------------------

    async fn create_snapshot(
        &self,
        request: Request<pb::CreateSnapshotRequest>,
    ) -> Result<Response<pb::SnapshotInfo>, Status> {
        let req = request.into_inner();
        let opts = crate::protocol::SnapshotOpts {
            label: if req.label.is_empty() {
                None
            } else {
                Some(req.label)
            },
        };

        match self.sandbox.snapshot(&req.sandbox_id, opts).await {
            Ok(info) => Ok(Response::new(info.to_proto())),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn restore_snapshot(
        &self,
        request: Request<pb::RestoreSnapshotRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        match self.sandbox.restore(&req.sandbox_id, &req.snapshot_id).await {
            Ok(()) => Ok(Response::new(())),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn list_snapshots(
        &self,
        _request: Request<pb::ListSnapshotsRequest>,
    ) -> Result<Response<pb::ListSnapshotsResponse>, Status> {
        Err(Status::unimplemented("list snapshots not yet implemented"))
    }

    // -----------------------------------------------------------------------
    // Volumes
    // -----------------------------------------------------------------------

    async fn create_volume(
        &self,
        request: Request<pb::CreateVolumeRequest>,
    ) -> Result<Response<pb::VolumeInfo>, Status> {
        let req = request.into_inner();
        let opts = crate::protocol::VolumeCreateOpts {
            name: req.name,
            size_mb: if req.size_mb > 0 {
                Some(req.size_mb)
            } else {
                None
            },
        };

        match self.volume.create(opts).await {
            Ok(info) => Ok(Response::new(info.to_proto())),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    async fn get_volume(
        &self,
        request: Request<pb::GetVolumeRequest>,
    ) -> Result<Response<pb::VolumeInfo>, Status> {
        let id = request.into_inner().id;
        match self.volume.get(&id).await {
            Some(info) => Ok(Response::new(info.to_proto())),
            None => Err(Status::not_found(format!("volume {} not found", id))),
        }
    }

    async fn list_volumes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pb::ListVolumesResponse>, Status> {
        let volumes = self.volume.list().await;
        Ok(Response::new(pb::ListVolumesResponse {
            volumes: volumes.into_iter().map(|v| v.to_proto()).collect(),
        }))
    }

    async fn remove_volume(
        &self,
        request: Request<pb::RemoveVolumeRequest>,
    ) -> Result<Response<()>, Status> {
        let id = request.into_inner().id;
        match self.volume.remove(&id).await {
            Ok(()) => Ok(Response::new(())),
            Err(msg) if msg.contains("mounted") => {
                Err(Status::failed_precondition(msg))
            }
            Err(msg) => Err(Status::not_found(msg)),
        }
    }

    // -----------------------------------------------------------------------
    // Egress
    // -----------------------------------------------------------------------

    async fn get_egress_log(
        &self,
        request: Request<pb::GetEgressLogRequest>,
    ) -> Result<Response<pb::EgressLogResponse>, Status> {
        let id = request.into_inner().sandbox_id;
        let entries = self.sandbox.egress_log(&id).await;
        Ok(Response::new(pb::EgressLogResponse {
            entries: entries.into_iter().map(|e| e.to_proto()).collect(),
        }))
    }

    // -----------------------------------------------------------------------
    // Health
    // -----------------------------------------------------------------------

    async fn get_health(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pb::HealthStatus>, Status> {
        let sandboxes = self.sandbox.list().await;
        Ok(Response::new(pb::HealthStatus {
            status: "healthy".into(),
            uptime_seconds: self.started_at.elapsed().as_secs(),
            sandbox_count: sandboxes.len() as u32,
            checked_at: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
        }))
    }

    async fn get_info(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pb::DaemonInfo>, Status> {
        Ok(Response::new(pb::DaemonInfo {
            version: super::VERSION.into(),
            platform: std::env::consts::OS.into(),
            backend: "libkrun".into(),
            arch: std::env::consts::ARCH.into(),
        }))
    }
}
```
-e 
### `ward-core/src/backend/mod.rs`

```rust
pub mod image;
pub mod krunvm;

pub use krunvm::KrunvmBackend;

use crate::protocol::{ProcessInfo, StreamEvent};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Error, Debug)]
pub enum BackendError {
    #[error("hardware virtualization not available (requires HVF on macOS or KVM on Linux)")]
    VirtualizationUnavailable,
    #[error("libkrun error: {0}")]
    LibkrunError(String),
    #[error("sandbox {0} not found")]
    SandboxNotFound(String),
    #[error("sandbox creation failed: {0}")]
    CreateFailed(String),
    #[error("process {0} not found in sandbox {1}")]
    ProcessNotFound(String, String),
    #[error("image error: {0}")]
    ImageError(String),
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// Handle to a running process inside a sandbox.
pub struct ProcessHandle {
    pub info: ProcessInfo,
    pub event_rx: mpsc::Receiver<StreamEvent>,
}
```
-e 
### `ward-core/src/backend/krunvm.rs`

```rust
//! Safe wrapper around `krun-sys` for managing libkrun microVMs.
//!
//! All `unsafe` calls to `krun-sys` are confined to this module.
//! The rest of ward-core interacts only with the safe `MicroVM` and
//! `KrunvmBackend` types.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use super::image::ImageStore;
use super::{BackendError, ProcessHandle, Result};
use crate::protocol::*;

// ---------------------------------------------------------------------------
// MicroVM: safe wrapper around a single libkrun VM context
// ---------------------------------------------------------------------------

/// Safe handle to a libkrun microVM context.
/// Creates a VM on construction, frees it on drop.
struct MicroVM {
    ctx: u32,
}

impl MicroVM {
    /// Create a new microVM context with the given resource limits.
    fn new(cpus: u8, memory_mb: u32) -> Result<Self> {
        let ctx = unsafe { krun_sys::krun_create_ctx() };
        if ctx < 0 {
            return Err(BackendError::LibkrunError(format!(
                "krun_create_ctx returned {}",
                ctx
            )));
        }
        let ctx = ctx as u32;

        let ret = unsafe { krun_sys::krun_set_vm_config(ctx, cpus, memory_mb) };
        if ret < 0 {
            unsafe { krun_sys::krun_free_ctx(ctx) };
            return Err(BackendError::LibkrunError(format!(
                "krun_set_vm_config returned {}",
                ret
            )));
        }

        Ok(Self { ctx })
    }

    /// Set the root filesystem path for the VM.
    fn set_root(&self, path: &str) -> Result<()> {
        let c_path =
            CString::new(path).map_err(|e| BackendError::LibkrunError(e.to_string()))?;
        let ret = unsafe { krun_sys::krun_set_root(self.ctx, c_path.as_ptr()) };
        if ret < 0 {
            return Err(BackendError::LibkrunError(format!(
                "krun_set_root returned {}",
                ret
            )));
        }
        Ok(())
    }

    /// Set the working directory inside the VM.
    fn set_workdir(&self, path: &str) -> Result<()> {
        let c_path =
            CString::new(path).map_err(|e| BackendError::LibkrunError(e.to_string()))?;
        let ret = unsafe { krun_sys::krun_set_workdir(self.ctx, c_path.as_ptr()) };
        if ret < 0 {
            return Err(BackendError::LibkrunError(format!(
                "krun_set_workdir returned {}",
                ret
            )));
        }
        Ok(())
    }

    /// Set the executable, arguments, and environment for the VM.
    fn set_exec(&self, exec_path: &str, args: &[&str], env: &[String]) -> Result<()> {
        let c_exec =
            CString::new(exec_path).map_err(|e| BackendError::LibkrunError(e.to_string()))?;

        let c_args: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a).unwrap())
            .collect();
        let c_arg_ptrs: Vec<*const i8> = c_args
            .iter()
            .map(|a| a.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let c_env: Vec<CString> = env
            .iter()
            .map(|e| CString::new(e.as_str()).unwrap())
            .collect();
        let c_env_ptrs: Vec<*const i8> = c_env
            .iter()
            .map(|e| e.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let ret = unsafe {
            krun_sys::krun_set_exec(
                self.ctx,
                c_exec.as_ptr(),
                c_arg_ptrs.as_ptr(),
                c_env_ptrs.as_ptr(),
            )
        };

        if ret < 0 {
            return Err(BackendError::LibkrunError(format!(
                "krun_set_exec returned {}",
                ret
            )));
        }
        Ok(())
    }

    /// Map host directories into the VM as volumes.
    fn set_mapped_volumes(&self, volumes: &[String]) -> Result<()> {
        let c_vols: Vec<CString> = volumes
            .iter()
            .map(|v| CString::new(v.as_str()).unwrap())
            .collect();
        let c_vol_ptrs: Vec<*const i8> = c_vols
            .iter()
            .map(|v| v.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        let ret =
            unsafe { krun_sys::krun_set_mapped_volumes(self.ctx, c_vol_ptrs.as_ptr()) };
        if ret < 0 {
            return Err(BackendError::LibkrunError(format!(
                "krun_set_mapped_volumes returned {}",
                ret
            )));
        }
        Ok(())
    }

    /// Start the VM and enter it. Blocks until the VM exits.
    /// Returns the exit code of the process inside the VM.
    fn start_enter(self) -> Result<i32> {
        let ret = unsafe { krun_sys::krun_start_enter(self.ctx) };
        // After start_enter, the context is consumed. Prevent Drop from freeing it.
        std::mem::forget(self);
        Ok(ret)
    }
}

impl Drop for MicroVM {
    fn drop(&mut self) {
        unsafe {
            krun_sys::krun_free_ctx(self.ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// KrunvmBackend: manages multiple sandboxes
// ---------------------------------------------------------------------------

struct SandboxState {
    info: SandboxInfo,
    egress: EgressPolicy,
}

/// Backend that manages sandboxes as libkrun microVMs.
pub struct KrunvmBackend {
    sandboxes: Arc<RwLock<HashMap<String, SandboxState>>>,
    images: ImageStore,
    data_dir: String,
}

impl KrunvmBackend {
    pub fn new(data_dir: &str) -> Self {
        Self {
            sandboxes: Arc::new(RwLock::new(HashMap::new())),
            images: ImageStore::new(&format!("{}/images", data_dir)),
            data_dir: data_dir.to_string(),
        }
    }

    /// Verify that libkrun can create a VM context on this platform.
    pub async fn check_available(&self) -> Result<()> {
        let ctx = unsafe { krun_sys::krun_create_ctx() };
        if ctx < 0 {
            return Err(BackendError::VirtualizationUnavailable);
        }
        unsafe {
            krun_sys::krun_free_ctx(ctx as u32);
        }
        info!("libkrun available, hardware virtualization confirmed");
        Ok(())
    }

    /// Create a new sandbox from an OCI image.
    pub async fn create(&self, opts: CreateOpts) -> Result<SandboxInfo> {
        let opts = opts.with_defaults();
        let id = format!("ward_{}", &Uuid::new_v4().to_string()[..8]);

        // Ensure the image is pulled and unpacked
        let rootfs_path = self
            .images
            .ensure_image(&opts.image)
            .await
            .map_err(|e| BackendError::ImageError(e.to_string()))?;

        // Build volume mappings
        let volume_strings: Vec<String> = opts
            .mounts
            .iter()
            .map(|m| format!("{}:{}", m.source, m.target))
            .collect();

        // Build environment
        let env_strings: Vec<String> = opts
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Create and configure the microVM
        let vm = MicroVM::new(opts.resources.cpus as u8, opts.resources.memory_mb)?;
        vm.set_root(&rootfs_path)?;

        if !volume_strings.is_empty() {
            vm.set_mapped_volumes(&volume_strings)?;
        }

        // Default to /bin/sh if no exec is specified at create time
        vm.set_exec("/bin/sh", &[], &env_strings)?;

        // TODO: spawn the VM in a background tokio task
        // vm.start_enter() blocks, so it must run on a dedicated thread
        // via tokio::task::spawn_blocking or a dedicated thread pool.
        // The sandbox stays "running" and accepts exec/run calls.

        let now = Utc::now();
        let expires_at = now
            + chrono::Duration::seconds(opts.resources.timeout_seconds as i64);

        let info = SandboxInfo {
            id: id.clone(),
            status: SandboxStatus::Running,
            image: opts.image.clone(),
            created_at: now,
            ip_address: None,
            resources: opts.resources,
            expires_at: Some(expires_at),
        };

        self.sandboxes.write().await.insert(
            id,
            SandboxState {
                info: info.clone(),
                egress: opts.egress,
            },
        );

        Ok(info)
    }

    /// Execute a shell command inside a sandbox.
    pub async fn exec(&self, sandbox_id: &str, opts: ExecOpts) -> Result<ProcessHandle> {
        {
            let sandboxes = self.sandboxes.read().await;
            if !sandboxes.contains_key(sandbox_id) {
                return Err(BackendError::SandboxNotFound(sandbox_id.to_string()));
            }
        }

        let pid = format!("exec_{}", &Uuid::new_v4().to_string()[..8]);
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // TODO: execute command inside the running microVM
        // This requires either:
        // 1. An agent process inside the VM that accepts commands over vsock
        // 2. SSH into the VM
        // 3. A new libkrun API for exec-in-running-VM
        //
        // For now, each exec creates a new short-lived microVM that runs
        // the command and exits. This is the krunvm model.

        let sandbox_id_owned = sandbox_id.to_string();
        tokio::task::spawn_blocking(move || {
            // TODO: create a new MicroVM, set_exec with the command, start_enter
            // Stream stdout/stderr via tx
            let _ = tx.blocking_send(StreamEvent {
                event_type: "exit".into(),
                line: None,
                code: Some(0),
                timestamp: Utc::now(),
                duration_ms: None,
            });
        });

        Ok(ProcessHandle {
            info: ProcessInfo {
                pid,
                sandbox_id: sandbox_id.to_string(),
                status: "running".into(),
            },
            event_rx: rx,
        })
    }

    /// Execute a code string inside a sandbox.
    pub async fn run(&self, sandbox_id: &str, opts: RunOpts) -> Result<ProcessHandle> {
        let runtimes = default_runtimes();
        let runtime = runtimes.get(opts.language.as_str()).ok_or_else(|| {
            BackendError::CreateFailed(format!("unsupported language: {}", opts.language))
        })?;

        let filename = format!("/tmp/ward_run{}", runtime.extension);
        let write_cmd = format!(
            "cat > {} << 'WARD_EOF'\n{}\nWARD_EOF",
            filename, opts.code
        );
        let exec_cmd = format!("{} {}", runtime.command, filename);
        let full_cmd = format!("{} && {}", write_cmd, exec_cmd);

        self.exec(
            sandbox_id,
            ExecOpts {
                command: vec!["/bin/sh".into(), "-c".into(), full_cmd],
                working_dir: None,
                env: Default::default(),
            },
        )
        .await
    }

    pub async fn get(&self, id: &str) -> Result<SandboxInfo> {
        self.sandboxes
            .read()
            .await
            .get(id)
            .map(|s| s.info.clone())
            .ok_or_else(|| BackendError::SandboxNotFound(id.to_string()))
    }

    pub async fn list(&self) -> Vec<SandboxInfo> {
        self.sandboxes
            .read()
            .await
            .values()
            .map(|s| s.info.clone())
            .collect()
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        // TODO: stop the running microVM if active
        self.sandboxes
            .write()
            .await
            .remove(id)
            .ok_or_else(|| BackendError::SandboxNotFound(id.to_string()))?;
        Ok(())
    }

    pub async fn snapshot(
        &self,
        _sandbox_id: &str,
        _opts: SnapshotOpts,
    ) -> Result<SnapshotInfo> {
        Err(BackendError::NotImplemented("snapshots".into()))
    }

    pub async fn restore(&self, _sandbox_id: &str, _snapshot_id: &str) -> Result<()> {
        Err(BackendError::NotImplemented("restore".into()))
    }

    pub async fn egress_policy(&self, sandbox_id: &str) -> Result<EgressPolicy> {
        self.sandboxes
            .read()
            .await
            .get(sandbox_id)
            .map(|s| s.egress.clone())
            .ok_or_else(|| BackendError::SandboxNotFound(sandbox_id.to_string()))
    }

    pub async fn shutdown(&self) {
        let ids: Vec<String> = self
            .sandboxes
            .read()
            .await
            .keys()
            .cloned()
            .collect();

        for id in ids {
            if let Err(e) = self.remove(&id).await {
                warn!(sandbox_id = %id, error = %e, "shutdown cleanup failed");
            }
        }
    }
}
```
-e 
### `ward-core/src/backend/image.rs`

```rust
//! OCI image management: pull, unpack, and cache container images.
//!
//! libkrun takes a local filesystem path as its root. This module handles
//! downloading OCI images from registries and unpacking their layers into
//! a local directory that libkrun can mount.

use std::path::{Path, PathBuf};

use tracing::info;

/// Local store for unpacked OCI images.
pub struct ImageStore {
    cache_dir: PathBuf,
}

impl ImageStore {
    pub fn new(cache_dir: &str) -> Self {
        Self {
            cache_dir: PathBuf::from(cache_dir),
        }
    }

    /// Ensure an image is pulled and unpacked locally.
    /// Returns the path to the unpacked root filesystem.
    ///
    /// If the image is already cached, returns immediately.
    /// Otherwise, pulls from the registry and unpacks.
    pub async fn ensure_image(&self, image_ref: &str) -> Result<String, String> {
        let image_dir = self.image_path(image_ref);

        if image_dir.exists() {
            info!(image = %image_ref, path = %image_dir.display(), "image cached");
            return Ok(image_dir.to_string_lossy().to_string());
        }

        info!(image = %image_ref, "pulling image");
        self.pull_and_unpack(image_ref, &image_dir).await?;

        Ok(image_dir.to_string_lossy().to_string())
    }

    /// Derive a local cache path from an image reference.
    /// "node:22-alpine" -> "{cache_dir}/node__22-alpine"
    fn image_path(&self, image_ref: &str) -> PathBuf {
        let sanitized = image_ref
            .replace('/', "__")
            .replace(':', "__");
        self.cache_dir.join(sanitized)
    }

    /// Pull an OCI image from a registry and unpack it.
    async fn pull_and_unpack(&self, image_ref: &str, dest: &Path) -> Result<(), String> {
        // TODO: implement using oci-distribution crate
        //
        // Steps:
        // 1. Parse image_ref into registry, repository, and tag
        //    e.g. "node:22-alpine" -> registry: "registry-1.docker.io",
        //         repository: "library/node", tag: "22-alpine"
        //
        // 2. Pull the manifest using oci_distribution::Client
        //
        // 3. Download each layer (tar.gz blobs)
        //
        // 4. Unpack layers in order into dest directory
        //    Each layer is applied on top of the previous one
        //    (handles whiteout files for deletions)
        //
        // 5. Extract the image config for default CMD, ENV, WORKDIR
        //    and store it alongside the rootfs as metadata.json

        std::fs::create_dir_all(dest)
            .map_err(|e| format!("failed to create image dir: {}", e))?;

        Err(format!(
            "image pulling not yet implemented for {}",
            image_ref
        ))
    }

    /// Remove a cached image.
    pub async fn remove_image(&self, image_ref: &str) -> Result<(), String> {
        let image_dir = self.image_path(image_ref);
        if image_dir.exists() {
            std::fs::remove_dir_all(&image_dir)
                .map_err(|e| format!("failed to remove image: {}", e))?;
        }
        Ok(())
    }

    /// List all cached images.
    pub async fn list_images(&self) -> Vec<String> {
        let mut images = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let name = entry
                        .file_name()
                        .to_string_lossy()
                        .replace("__", "/")
                        .replacen("/", ":", 1);
                    images.push(name);
                }
            }
        }
        images
    }
}
```
-e 
### `ward-core/src/egress/mod.rs`

```rust
mod proxy;
pub use proxy::{EgressProxy, LogEntry};
```
-e 
### `ward-core/src/egress/proxy.rs`

```rust
//! Per-sandbox egress proxy with domain-level allowlist filtering.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::protocol::{EgressMode, EgressPolicy};

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub sandbox_id: String,
    pub domain: String,
    pub port: String,
    pub allowed: bool,
    pub timestamp: DateTime<Utc>,
}

pub struct EgressProxy {
    policies: Arc<RwLock<HashMap<String, EgressPolicy>>>,
    log: Arc<RwLock<Vec<LogEntry>>>,
}

impl EgressProxy {
    pub fn new() -> Self {
        Self {
            policies: Arc::new(RwLock::new(HashMap::new())),
            log: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn set_policy(&self, sandbox_id: &str, policy: EgressPolicy) {
        self.policies
            .write()
            .await
            .insert(sandbox_id.to_string(), policy);
    }

    pub async fn remove_policy(&self, sandbox_id: &str) {
        self.policies.write().await.remove(sandbox_id);
    }

    pub async fn is_allowed(&self, sandbox_id: &str, domain: &str) -> bool {
        let policies = self.policies.read().await;
        let Some(policy) = policies.get(sandbox_id) else {
            return false; // no policy = deny by default
        };

        match policy.mode {
            EgressMode::Open => true,
            EgressMode::Deny | EgressMode::Unset => false,
            EgressMode::Allowlist => Self::matches_domain(domain, &policy.domains),
        }
    }

    fn matches_domain(domain: &str, allowlist: &[String]) -> bool {
        let domain = domain.to_lowercase().trim_end_matches('.').to_string();

        for pattern in allowlist {
            let pattern = pattern.to_lowercase().trim_end_matches('.').to_string();

            if pattern == domain {
                return true;
            }

            // *.example.com matches sub.example.com
            if let Some(suffix) = pattern.strip_prefix('*') {
                if domain.ends_with(&suffix) {
                    return true;
                }
            }
        }

        false
    }

    pub async fn record_attempt(
        &self,
        sandbox_id: &str,
        domain: &str,
        port: &str,
        allowed: bool,
    ) {
        self.log.write().await.push(LogEntry {
            sandbox_id: sandbox_id.to_string(),
            domain: domain.to_string(),
            port: port.to_string(),
            allowed,
            timestamp: Utc::now(),
        });
    }

    pub async fn get_log(&self, sandbox_id: &str) -> Vec<LogEntry> {
        self.log
            .read()
            .await
            .iter()
            .filter(|e| e.sandbox_id == sandbox_id)
            .cloned()
            .collect()
    }
}
```
-e 
### `ward-core/src/sandbox/mod.rs`

```rust
mod manager;
pub use manager::SandboxManager;
```
-e 
### `ward-core/src/sandbox/manager.rs`

```rust
//! Coordinates sandbox lifecycle between the API layer, krunvm backend,
//! egress proxy, and timeout enforcement.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::backend::{KrunvmBackend, ProcessHandle, Result};
use crate::egress::EgressProxy;
use crate::protocol::*;

pub struct SandboxManager {
    backend: Arc<KrunvmBackend>,
    egress: Arc<EgressProxy>,
    timers: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
}

impl SandboxManager {
    pub fn new(backend: Arc<KrunvmBackend>, egress: Arc<EgressProxy>) -> Self {
        Self {
            backend,
            egress,
            timers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn create(&self, opts: CreateOpts) -> Result<SandboxInfo> {
        let opts = opts.with_defaults();
        let timeout_secs = opts.resources.timeout_seconds;
        let egress_policy = opts.egress.clone();

        let info = self.backend.create(opts).await?;
        let sandbox_id = info.id.clone();

        // Configure egress
        self.egress.set_policy(&sandbox_id, egress_policy).await;

        // Set up timeout
        if timeout_secs > 0 {
            let backend = Arc::clone(&self.backend);
            let egress = Arc::clone(&self.egress);
            let id = sandbox_id.clone();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;
                warn!(sandbox_id = %id, "sandbox timed out, removing");
                let _ = backend.remove(&id).await;
                egress.remove_policy(&id).await;
            });
            self.timers.write().await.insert(sandbox_id, handle);
        }

        Ok(info)
    }

    pub async fn get(&self, id: &str) -> Result<SandboxInfo> {
        self.backend.get(id).await
    }

    pub async fn list(&self) -> Vec<SandboxInfo> {
        self.backend.list().await
    }

    pub async fn exec(&self, sandbox_id: &str, opts: ExecOpts) -> Result<ProcessHandle> {
        self.backend.exec(sandbox_id, opts).await
    }

    pub async fn run(&self, sandbox_id: &str, opts: RunOpts) -> Result<ProcessHandle> {
        self.backend.run(sandbox_id, opts).await
    }

    pub async fn snapshot(
        &self,
        sandbox_id: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotInfo> {
        self.backend.snapshot(sandbox_id, opts).await
    }

    pub async fn restore(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()> {
        self.backend.restore(sandbox_id, snapshot_id).await
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        // Cancel timeout timer
        if let Some(handle) = self.timers.write().await.remove(id) {
            handle.abort();
        }

        // Remove egress policy
        self.egress.remove_policy(id).await;

        // Remove sandbox
        self.backend.remove(id).await
    }

    pub async fn egress_log(&self, sandbox_id: &str) -> Vec<crate::egress::LogEntry> {
        self.egress.get_log(sandbox_id).await
    }

    pub async fn shutdown(&self) {
        // Cancel all timers
        let mut timers = self.timers.write().await;
        for (_, handle) in timers.drain() {
            handle.abort();
        }

        // Shut down all sandboxes
        self.backend.shutdown().await;
    }
}
```
-e 
### `ward-core/src/volume/mod.rs`

```rust
mod manager;
pub use manager::VolumeManager;
```
-e 
### `ward-core/src/volume/manager.rs`

```rust
//! Daemon-managed shared volumes mountable across multiple sandboxes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::protocol::{VolumeCreateOpts, VolumeInfo};

struct VolumeState {
    info: VolumeInfo,
    mounted_by: HashSet<String>,
}

pub struct VolumeManager {
    volumes: Arc<RwLock<HashMap<String, VolumeState>>>,
    data_dir: PathBuf,
}

impl VolumeManager {
    pub fn new(data_dir: &str) -> Self {
        Self {
            volumes: Arc::new(RwLock::new(HashMap::new())),
            data_dir: PathBuf::from(data_dir).join("volumes"),
        }
    }

    pub async fn create(&self, opts: VolumeCreateOpts) -> std::io::Result<VolumeInfo> {
        let id = format!("vol_{}", &Uuid::new_v4().to_string()[..8]);
        let volume_path = self.data_dir.join(&id);
        tokio::fs::create_dir_all(&volume_path).await?;

        let info = VolumeInfo {
            id: id.clone(),
            name: opts.name,
            size_mb: opts.size_mb,
            created_at: Utc::now(),
            mount_path: volume_path.to_string_lossy().to_string(),
        };

        self.volumes.write().await.insert(
            id,
            VolumeState {
                info: info.clone(),
                mounted_by: HashSet::new(),
            },
        );

        Ok(info)
    }

    pub async fn get(&self, id: &str) -> Option<VolumeInfo> {
        self.volumes.read().await.get(id).map(|v| v.info.clone())
    }

    pub async fn list(&self) -> Vec<VolumeInfo> {
        self.volumes.read().await.values().map(|v| v.info.clone()).collect()
    }

    pub async fn remove(&self, id: &str) -> std::result::Result<(), String> {
        let mut volumes = self.volumes.write().await;
        let Some(state) = volumes.get(id) else {
            return Err(format!("volume {} not found", id));
        };

        if !state.mounted_by.is_empty() {
            return Err(format!(
                "volume {} is mounted by {} sandbox(es)",
                id,
                state.mounted_by.len()
            ));
        }

        let path = &state.info.mount_path;
        let _ = tokio::fs::remove_dir_all(path).await;
        volumes.remove(id);
        Ok(())
    }

    pub async fn register_mount(&self, volume_id: &str, sandbox_id: &str) {
        if let Some(state) = self.volumes.write().await.get_mut(volume_id) {
            state.mounted_by.insert(sandbox_id.to_string());
        }
    }

    pub async fn deregister_mount(&self, volume_id: &str, sandbox_id: &str) {
        if let Some(state) = self.volumes.write().await.get_mut(volume_id) {
            state.mounted_by.remove(sandbox_id);
        }
    }
}
```
-e 
### `wardd/Cargo.toml`

```toml
[package]
name = "wardd"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[[bin]]
name = "wardd"
path = "src/main.rs"

[dependencies]
ward-core = { path = "../ward-core" }
tokio = { version = "1", features = ["full"] }
tokio-stream = "0.1"
tonic = "0.12"
tower = "0.5"
hyper-util = "0.1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
```
-e 
### `wardd/src/main.rs`

```rust
//! Ward daemon: starts the gRPC server on a Unix socket, verifies libkrun
//! is available, and manages sandbox lifecycle until terminated.
//!
//! Usage:
//!   wardd                            # start with defaults
//!   WARD_SOCKET=/path/to.sock wardd
//!   WARD_LOG_LEVEL=debug wardd

use std::sync::Arc;
use std::time::Instant;

use tokio::net::UnixListener;
use tokio::signal;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{error, info};

use ward_core::backend::KrunvmBackend;
use ward_core::config::Config;
use ward_core::egress::EgressProxy;
use ward_core::grpc::WardGrpcServer;
use ward_core::pb::ward_server::WardServer;
use ward_core::sandbox::SandboxManager;
use ward_core::volume::VolumeManager;

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();

    // Structured logging
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.log_level));

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter)
        .init();

    // Ensure data directories
    if let Err(e) = cfg.ensure_dirs() {
        error!(error = %e, "failed to create data directories");
        std::process::exit(1);
    }

    // Verify libkrun is available
    let backend = Arc::new(KrunvmBackend::new(&cfg.data_dir));
    if let Err(e) = backend.check_available().await {
        error!(error = %e, "isolation backend unavailable");
        std::process::exit(1);
    }

    // Initialise components
    let egress = Arc::new(EgressProxy::new());
    let sandbox_mgr = Arc::new(SandboxManager::new(
        Arc::clone(&backend),
        Arc::clone(&egress),
    ));
    let volume_mgr = Arc::new(VolumeManager::new(&cfg.data_dir));

    let grpc_server = WardGrpcServer {
        sandbox: Arc::clone(&sandbox_mgr),
        volume: Arc::clone(&volume_mgr),
        started_at: Instant::now(),
    };

    // Remove stale socket
    let _ = std::fs::remove_file(&cfg.socket_path);

    // Bind Unix socket
    let uds = match UnixListener::bind(&cfg.socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!(path = %cfg.socket_path, error = %e, "failed to bind socket");
            std::process::exit(1);
        }
    };

    // Set socket permissions (owner only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &cfg.socket_path,
            std::fs::Permissions::from_mode(0o600),
        );
    }

    info!(
        socket = %cfg.socket_path,
        version = ward_core::grpc::VERSION,
        pid = std::process::id(),
        "ward daemon started (gRPC over Unix socket)"
    );

    let uds_stream = UnixListenerStream::new(uds);

    // Serve gRPC
    let server = Server::builder()
        .add_service(WardServer::new(grpc_server))
        .serve_with_incoming_shutdown(uds_stream, async {
            signal::ctrl_c().await.ok();
            info!("shutting down");
        });

    if let Err(e) = server.await {
        error!(error = %e, "server error");
    }

    // Graceful shutdown
    sandbox_mgr.shutdown().await;

    // Clean up socket
    let _ = std::fs::remove_file(&cfg.socket_path);

    info!("ward daemon stopped");
}
```
-e 
### `ward-cli/Cargo.toml`

```toml
[package]
name = "ward-cli"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[[bin]]
name = "ward"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```
-e 
### `ward-cli/src/main.rs`

```rust
//! Ward CLI: communicates with the ward daemon (wardd) over a Unix socket.
//!
//! Usage:
//!   ward create [--image alpine] [--egress allow:npmjs.org]
//!   ward list
//!   ward exec <sandbox-id> -- npm test
//!   ward run <sandbox-id> --lang python --code 'print("hi")'
//!   ward logs <sandbox-id> <pid>
//!   ward snapshot <sandbox-id> [--label name]
//!   ward restore <sandbox-id> --snapshot <snap-id>
//!   ward remove <sandbox-id>
//!   ward volume create --name shared-data
//!   ward volume list
//!   ward volume remove <volume-id>
//!   ward health
//!   ward info

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ward", about = "Isolated execution environments")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new sandbox
    Create {
        #[arg(long, default_value = "alpine:latest")]
        image: String,
        #[arg(long)]
        cpus: Option<u32>,
        #[arg(long)]
        memory: Option<u32>,
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// List all sandboxes
    #[command(alias = "ls")]
    List,
    /// Execute a command in a sandbox
    Exec {
        sandbox_id: String,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Run a code string in a sandbox
    Run {
        sandbox_id: String,
        #[arg(long)]
        lang: String,
        #[arg(long)]
        code: String,
    },
    /// Stream logs from a process
    Logs {
        sandbox_id: String,
        pid: String,
    },
    /// Snapshot a sandbox
    Snapshot {
        sandbox_id: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Restore a sandbox from a snapshot
    Restore {
        sandbox_id: String,
        #[arg(long)]
        snapshot: String,
    },
    /// Remove a sandbox
    #[command(alias = "rm")]
    Remove {
        sandbox_id: String,
    },
    /// Volume management
    Volume {
        #[command(subcommand)]
        action: VolumeCommands,
    },
    /// Check daemon health
    Health,
    /// Show daemon info
    Info,
}

#[derive(Subcommand)]
enum VolumeCommands {
    /// Create a shared volume
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        size: Option<u32>,
    },
    /// List all volumes
    List,
    /// Remove a volume
    #[command(alias = "rm")]
    Remove {
        volume_id: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // TODO: implement each command by sending HTTP requests
    // to the daemon over the Unix socket.
    //
    // Example:
    //   let socket = std::env::var("WARD_SOCKET")
    //       .unwrap_or_else(|_| default_socket_path());
    //   let client = hyper_util::client::legacy::Client::unix(socket);
    //   let resp = client.post("/v1/sandboxes").json(&opts).send().await?;

    match cli.command {
        Commands::Create { image, cpus, memory, timeout } => {
            println!("TODO: create sandbox (image={image})");
        }
        Commands::List => {
            println!("TODO: list sandboxes");
        }
        Commands::Exec { sandbox_id, command } => {
            println!("TODO: exec in {sandbox_id}: {:?}", command);
        }
        Commands::Run { sandbox_id, lang, code } => {
            println!("TODO: run {lang} in {sandbox_id}");
        }
        Commands::Logs { sandbox_id, pid } => {
            println!("TODO: stream logs for {sandbox_id}/{pid}");
        }
        Commands::Snapshot { sandbox_id, label } => {
            println!("TODO: snapshot {sandbox_id}");
        }
        Commands::Restore { sandbox_id, snapshot } => {
            println!("TODO: restore {sandbox_id} from {snapshot}");
        }
        Commands::Remove { sandbox_id } => {
            println!("TODO: remove {sandbox_id}");
        }
        Commands::Volume { action } => match action {
            VolumeCommands::Create { name, size } => {
                println!("TODO: create volume {name}");
            }
            VolumeCommands::List => {
                println!("TODO: list volumes");
            }
            VolumeCommands::Remove { volume_id } => {
                println!("TODO: remove volume {volume_id}");
            }
        },
        Commands::Health => {
            println!("TODO: health check");
        }
        Commands::Info => {
            println!("TODO: daemon info");
        }
    }
}
```
