# ADR-002: Language Choice

**Status:** Accepted
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward is a long-running daemon that manages concurrent sandbox lifecycles via libkrun, streaming I/O, enforcing timeouts, proxying egress, and cleaning up on failure. The language must support this workload, produce a single distributable binary, and align with the ecosystem Ward integrates with.

### Candidates evaluated

**Go:** Purpose-built for networked services. Goroutines map naturally to concurrent sandbox management. Excellent stdlib for HTTP, JSON, and sockets. Single static binary. Evaluated and ultimately passed over.

**Rust:** Compiler-enforced memory safety without garbage collection. Strong async runtime (tokio) for concurrent I/O. Same language as libkrun (the VMM ecosystem Ward wraps). Single static binary. Steeper learning curve but stronger long-term guarantees.

**C/C++:** Maximum control but manual memory management in a long-running daemon is a reliability risk. Rejected.

**Swift:** Native access to Apple frameworks but server-side ecosystem is thin and the project needs cross-platform support (macOS + Linux). Rejected.

## Decision

Ward is written in Rust.

### Rationale

1. **Same ecosystem as the VMM.** libkrun is Rust internally. If Ward ever needs to go deeper than the C-API bindings (contribute upstream, fork for customization), Rust gives native access with zero FFI overhead.

2. **Safety for a security-critical daemon.** Ward manages isolation boundaries. A memory bug in the daemon could compromise the isolation model. Rust's borrow checker eliminates use-after-free, double-free, and data races at compile time.

3. **Error handling.** The daemon's job is propagating errors from sandbox operations (VM creation failures, exec timeouts, egress denials) to the SDK. Rust's `Result<T, E>` with `?` propagation is purpose-built for this. Every error path is explicit and compiler-checked.

4. **Async concurrency.** tokio provides lightweight tasks, channels, timers, and an async gRPC server (tonic). The daemon's concurrency pattern (manage N independent sandbox lifecycles, each with I/O streams and timeouts) maps directly to tokio tasks.

5. **Single static binary.** `cargo build --release` produces a binary with minimal runtime dependencies. On macOS, the binary links to system frameworks only. On Linux, musl targets produce fully static binaries.

6. **Build to last.** Rust's edition system allows language evolution without breaking existing code. Cargo's dependency management and semver enforcement provide stability.

### What Rust costs

- Slower initial development vs Go (estimated 30-50% slower for the first month).
- Async Rust has friction (pinning, lifetimes across await points). The daemon's async surface is manageable: gRPC server, subprocess I/O, timers.
- Longer compile times (~30-60 seconds for a clean build). Incremental builds are fast.
- Smaller contributor pool than Go. Accepted tradeoff per project goals.

## Consequences

- The daemon is a Cargo workspace with three crates: `ward-daemon` (daemon binary, formerly `wardd`), `ward-cli` (CLI binary), `ward-core` (shared library).
- The binary inside `ward-daemon/` is still named `wardd` for invocation continuity (`systemctl start wardd`).
- MSRV is pinned to Rust 1.88 (workspace `rust-version`); raised from 1.85 when the OCI image-pull stack (`oci-client` → `jsonwebtoken`) required it.
- Cross-compilation targets: `aarch64-apple-darwin` (macOS), `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`.
- Key dependencies: `tokio` (async runtime), `tonic` (gRPC server), `prost`/`prost-types` (protobuf), `async-trait` (Backend trait), `tracing` (structured logging).
- libkrun integration is via hand-maintained `unsafe extern "C"` declarations in `ward-core/src/backend/krun_ffi.rs`, gated behind the `krunvm` cargo feature. Default builds use a stub backend; `--features krunvm` links real libkrun. See ADR-003.
