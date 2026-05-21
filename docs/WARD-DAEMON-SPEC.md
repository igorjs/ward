# Ward Daemon Specification

**This file is a historical placeholder.** The canonical specification now
lives in [`SPEC.md`](SPEC.md) with per-ADR files under
[`adr/`](adr/).

## Why the split

The original `WARD-DAEMON-SPEC.md` was a single 3,000-line document
containing 10 ADRs, the full protobuf schema, and a complete Rust
source skeleton. Over the course of 2026-05, the codebase drifted from
the embedded source skeleton (intentionally — it evolved through 4+
months of implementation, adding the cross-sandbox broker, the
Backend trait abstraction, capacity caps, and a full test suite).

Maintaining a 3,000-line spec alongside the moving source code was
strictly worse than two things at once:

1. **The ADRs** capture *why* decisions were made. They evolve slowly
   and benefit from clean per-ADR diffs. They live in `adr/`.
2. **The source code** captures *what* the system actually does. It
   evolves continuously and is the canonical reference for current
   behaviour.

## Where to read

| If you want to know… | Read… |
|----------------------|-------|
| Why ward exists, what's in/out of scope | [ADR-001](adr/001-project-scope.md) |
| Why Rust, why libkrun, why gRPC | [ADR-002–004](adr/002-language.md) |
| What licences cover what | [ADR-006](adr/006-licensing.md) |
| What platforms ward runs on | [ADR-007](adr/007-platform-support.md) |
| How egress / snapshots / volumes work | [ADR-008–010](adr/008-egress-control.md) |
| How cross-sandbox pub/sub works | [ADR-011](adr/011-cross-sandbox-comms.md) |
| The Backend trait abstraction | [ADR-012](adr/012-backend-trait.md) |
| The wire protocol | [`proto/ward.proto`](../proto/ward.proto) |
| The actual Rust source | the repo — `ward-core/`, `ward-daemon/`, `ward-cli/` |
