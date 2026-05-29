# ADR-014: WASM Backend Alongside libkrun

**Status:** Proposed (no implementation yet)
**Date:** 2026-05-27
**Authors:** Igor

## Context

ward's primary isolation backend is libkrun ([ADR-003](003-isolation-backend.md))
via hardware-virtualised microVMs, sub-second boot, real Linux kernel
per sandbox. The Backend trait ([ADR-012](012-backend-trait.md)) abstracts
this so future backends can plug in.

A growing class of workloads doesn't need a full microVM:

- Untrusted JS/Python/Rust **function** snippets from an LLM ("compute
  the sum of this list", "transform this JSON") that need code
  evaluation but no OS surface, no syscalls beyond memory + math, no
  network.
- Plugin systems where third-party code extends a host application.
- CI step isolation for short pure-computation steps.

For these, a microVM is overkill: ~500ms cold boot, ~100MB memory floor,
overhead vs the actual work done. **WASM via wasmtime** gives:

- <10ms instantiation
- <1MB memory floor
- Sandboxed by construction (linear memory, no syscalls except those
  the host explicitly imports)
- Cross-language (anything that compiles to WASM)

The threat models differ:

| Concern | libkrun (hardware) | WASM (software) |
|---|---|---|
| Kernel isolation | Yes (own Linux kernel) | N/A (no kernel) |
| Memory safety | Hardware-virtualised | Linear-memory-isolated, software-enforced |
| Side-channel resistance | Strong (separate CPU state) | Weaker (Spectre-class via shared CPU) |
| Privileged escapes | Hypervisor bug (rare) | Wasmtime / V8 bug (more frequent historically) |

WASM is not a replacement for libkrun when you genuinely need a kernel
(running `apt install`, running an untrusted shell that calls `ls`,
running anything that needs syscalls). It's a complement.

## Decision

**Add a `WasmtimeBackend` implementing the `Backend` trait, alongside
the existing `KrunvmBackend`. Choose at sandbox-create time via a new
`isolation` field on `CreateSandboxRequest`.**

Status: proposed pending a concrete use case lands. Implementation
estimate: 2-4 weeks plus design iteration.

### Trait impl surface

WASM has no fork/process model in the OS sense. The Backend trait
methods map awkwardly. The honest answer: WASM only supports a subset:

| Method | WASM behaviour |
|---|---|
| `create_sandbox` | Instantiate a wasmtime `Engine` + `Store`. Module URL stored, not yet loaded. |
| run a command | Map `command[0]` to a wasm export name. Args / env passed via WASI. |
| `kill_process(pid)` | Epoch interruption: `Engine::increment_epoch()` after `Config::epoch_interruption(true)`. (The older `Store::interrupt_handle().interrupt()` API is removed in current wasmtime releases.) |
| `get_sandbox`, `list_sandboxes`, `remove_sandbox` | Same shape as libkrun |
| `create_snapshot`, `restore_snapshot` | **Unsupported**: wasmtime has no checkpoint API today. Return `Unimplemented`. |

Some operations don't make sense:
- Bind mounts → WASI dirs (different mechanism but conceptually similar)
- Volumes → not supported; WASM doesn't have a block device concept
- Egress proxy → not needed; WASM has no network by default. If the
  host imports a network function, that becomes the egress boundary.

### Module sourcing

WASM modules are not OCI images. Three options:

1. **OCI artifact spec.** WASM modules wrapped in OCI manifests
   (wasi-oci specifies this). Reuses the OCI registry infrastructure
   ward already has.
2. **Bare module URL.** `wasm:https://example.com/module.wasm`:
   simpler but no signing / discovery story.
3. **Local file.** `wasm:///path/to/module.wasm` for development.

Pick option 1 (OCI artifact spec) as the primary path: lets ward's
existing image-pull machinery work for WASM with minimal change.

### `CreateSandboxRequest.isolation` field

A new optional enum on the request:

```proto
enum Isolation {
    DEFAULT = 0;  // libkrun if image is OCI, error otherwise
    MICROVM = 1;  // libkrun (explicit)
    WASM = 2;     // wasmtime
}
```

Backwards-compatible: existing clients omit the field, get the current
libkrun behaviour. WASM workloads set `isolation = WASM` + a wasm-OCI
reference in `image`.

## Why defer

- **No live use case from ward's current users.** "I want a faster
  sandbox for JS functions" hasn't come up. Building speculatively
  invites premature complexity.
- **WASM ecosystem is fast-moving.** Component model, WASI 0.3,
  WIT bindings; what's idiomatic today may be obsolete in 6 months.
  Waiting reduces churn.
- **The hard part is the API design, not the implementation.** Once
  the `Isolation` field and `WasmtimeBackend` skeleton are agreed,
  the actual code is a week. The 2-4-week estimate is mostly
  conformance testing + WASI surface decisions.

## Consequences

- The Backend trait abstraction in ADR-012 pays off: adding WASM is a
  new struct implementing the trait, not a refactor of `SandboxManager`
  or the gRPC layer.
- Users get a "use the right isolation for the workload" mental model:
  microVM when you need a kernel, WASM when you need a function.
- ward's threat model becomes more nuanced: different sandboxes
  on the same daemon may have different isolation strengths.
  [SECURITY.md](../../SECURITY.md) needs an "Isolation modes" section
  when this lands.
- Snapshot/restore is partially unsupported on WASM, breaking the
  `create_snapshot` contract. Either return `Unimplemented` (clean
  but introduces method-not-supported branching) or split the trait
  into core + snapshots (cleaner long-term but more invasive).

## Alternatives considered

- **gVisor or runsc instead of libkrun.** Software syscall interception;
  Linux-only; weaker isolation than hardware VM, stronger than WASM.
  Could fit between libkrun and WASM on the isolation/cost spectrum.
  Deferred: doesn't add enough vs the WASM + libkrun two-tier story.
- **Firecracker as the second backend.** Hardware VM like libkrun,
  Linux-only, more mature. Worth considering if ward expands server-side
  use cases significantly. Tracked separately.
- **Don't add WASM; let users compose ward with a separate WASM runtime.**
  Cleanest if WASM workloads are rare. Loses the unified
  CreateSandbox / run / Remove flow ward exists to provide.
