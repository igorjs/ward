# ADR-012: Backend Trait Abstraction

**Status:** Accepted (implemented in `ward-core/src/backend/mod.rs`)
**Date:** 2026-05-15
**Authors:** Igor

## Context

ADR-003 picked libkrun as the isolation backend. That decision is sound for ward's primary platforms (macOS arm64, Linux x86_64/arm64), but several questions are open:

- **Multi-platform coverage**: libkrun does not support Windows or Intel Macs. If ward needs broader coverage, a backend like Firecracker (Linux servers) or Apple's `Virtualization.framework` (full macOS support) may be required alongside or instead.
- **Snapshot support**: libkrun 1.10 lacks snapshot/restore in its public API (ADR-009). Firecracker has it. If the snapshot feature ramps in importance, swapping or supplementing the backend becomes attractive.
- **Maturity**: libkrun is younger than Firecracker. Production deployment patterns are less well-trodden.

Coupling `SandboxManager` to the concrete `KrunvmBackend` type makes swapping any of this a multi-week refactor. Doing it now, with one impl, is cheap insurance.

## Decision

`ward-core/src/backend/mod.rs` defines a `Backend` trait covering every operation `SandboxManager` invokes on a backend. `KrunvmBackend` implements it. `SandboxManager` holds `Arc<dyn Backend>`.

### Trait surface

```rust
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    // Sandbox lifecycle
    async fn create_sandbox(&self, id: String, opts: &CreateOpts) -> Result<SandboxInfo>;
    async fn get_sandbox(&self, id: &str) -> Result<SandboxInfo>;
    async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>>;
    async fn remove_sandbox(&self, id: &str) -> Result<()>;
    async fn count(&self) -> Result<usize>;

    // Process operations
    async fn exec(&self, sandbox_id: &str, command: Vec<String>,
                  working_dir: Option<String>, env: HashMap<String, String>)
        -> Result<ProcessHandle>;
    async fn kill_process(&self, sandbox_id: &str, pid: &str) -> Result<()>;

    // Snapshots
    async fn create_snapshot(&self, sandbox_id: &str, label: &str) -> Result<SnapshotInfo>;
    async fn restore_snapshot(&self, sandbox_id: &str, snapshot_id: &str) -> Result<()>;
    async fn list_snapshots(&self, sandbox_id: &str) -> Result<Vec<SnapshotInfo>>;
}
```

Ten methods. Everything `SandboxManager` needs and nothing more.

### Why `async-trait` (boxed futures) not native AFIT

Rust 1.85+ supports native `async fn` in trait, but trait-object support (`dyn Backend`) requires explicit `Send` bounds per method and breaks ergonomically. The `async-trait` macro boxes each method's future automatically. The per-call allocation is far smaller than the gRPC roundtrip happening on every call site. Generics (`B: Backend`) would viral through every test harness and tonic handler.

### Wiring

Three call sites:

- `ward-daemon/src/main.rs`: `let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::new(...));`
- `ward-core/tests/common/mod.rs` (integration test harness): same shape
- `ward-core/src/sandbox/manager.rs` (inline-test `build_manager` helper): same shape

`SandboxManager::new` accepts `Arc<dyn Backend>`. Swapping the backend changes one `let` binding and writes one new trait impl. Manager, gRPC server, broker, and all 387 tests are untouched.

### Private FFI helpers stay inherent

`KrunvmBackend`'s private FFI wrappers (`krun_create_ctx`, `krun_free_ctx`, `krun_apply_resources`, `krun_set_root`) remain inherent on `KrunvmBackend`, not in the trait. They're an implementation detail of one backend.

### What this enables

| Backend | Status | When to consider |
|---------|--------|------------------|
| `KrunvmBackend` (libkrun) | Implemented | Default for ward's supported platforms |
| `FirecrackerBackend` | Not implemented | Linux-only servers where snapshot/restore is must-have |
| `VirtualizationFrameworkBackend` | Not implemented | macOS Apple Silicon, Apple-blessed VMM, snapshot support |
| `MockBackend` | Implemented as stub inside `KrunvmBackend` when `krunvm` feature is off | Tests, CI, dev on unsupported platforms |
| `HybridBackend` | Not implemented | Linux uses Firecracker, macOS uses libkrun, abstracted behind trait |

The current "stub vs real libkrun" split is currently done via the cargo `krunvm` feature inside `KrunvmBackend` rather than via separate trait impls. The trait abstraction means either approach works going forward.

## Consequences

- One trait, one current impl. Cheap insurance against future backend swaps without speculative abstraction in other layers.
- The 10-method surface is exactly what `SandboxManager` calls — adding capabilities (e.g. live migration) means adding to the trait.
- Tests pass identically before and after the abstraction (387 tests, zero changes), which is the desired signal for a well-scoped refactor.
- `async-trait` adds one small per-call allocation. Negligible relative to gRPC + IPC costs already on every call.
- If we ever ship `ward` on Windows (via some future Hypervisor backend), it's a write-the-trait-impl + change-one-line task, not a manager-and-gRPC refactor.
