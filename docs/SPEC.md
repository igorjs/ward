# Ward Daemon: Specification

This document is the table of contents for the Ward daemon's architecture
decision records (ADRs). Each ADR captures a single design decision, its
context, and its consequences.

The **canonical source code is the repository itself.** Don't read the ADRs
expecting line-accurate code; read them to understand *why* the code is
shaped the way it is.

## Table of Contents

### Architecture decisions

- [ADR-001: Project Scope, Purpose, and Layering](adr/001-project-scope.md)
- [ADR-002: Language Choice (Rust)](adr/002-language.md)
- [ADR-003: Isolation Backend (libkrun)](adr/003-isolation-backend.md)
- [ADR-004: IPC Protocol (gRPC + protobuf)](adr/004-ipc-protocol.md)
- [ADR-005: SDK Strategy (generated clients + idiomatic wrappers)](adr/005-sdk-strategy.md)
- [ADR-006: Licensing (AGPL-3.0 daemon, Apache-2.0 SDKs)](adr/006-licensing.md)
- [ADR-007: Platform Support and Hardware Requirements](adr/007-platform-support.md)
- [ADR-008: Egress Control and Network Isolation](adr/008-egress-control.md)
- [ADR-009: Snapshots and State Management](adr/009-snapshots.md)
- [ADR-010: Shared Volumes](adr/010-shared-volumes.md)
- [ADR-011: Cross-Sandbox Communication (pub/sub broker)](adr/011-cross-sandbox-comms.md)
- [ADR-012: Backend Trait Abstraction](adr/012-backend-trait.md)

### How to read

- **ADR-001** establishes the boundary between the daemon, the SDKs, and the future remote management.
- **ADR-002–004** are the foundational tech choices (Rust, libkrun, gRPC).
- **ADR-005–006** cover SDK strategy and licensing.
- **ADR-007** covers platforms + feature-flag build modes.
- **ADR-008–010** are user-visible features: egress filtering, snapshots, shared volumes.
- **ADR-011** is the cross-sandbox pub/sub broker (added 2026-05-14).
- **ADR-012** is the Backend trait abstraction that decouples `SandboxManager` from the concrete VMM implementation (added 2026-05-15).

### How to add an ADR

1. Create `docs/adr/NNN-short-name.md`.
2. Use the header block: `Status`, `Date`, `Authors`. Status starts as `Proposed`, moves to `Accepted` when merged, may become `Superseded by ADR-NNN` later.
3. Three sections: Context (what's the problem), Decision (what we picked), Consequences (what it costs and what it enables).
4. Link from this table of contents.

## Companion projects

The Ward ecosystem spans more than this repo. References:

- `igorjs/ward` (this repo) — daemon (AGPL-3.0), CLI, registry tools
- `igorjs/ward-sdk-*` (future) — language-specific SDKs (Apache-2.0)
- `igorjs/internal-project` (private) — proprietary fleet management; not a derivative work of the daemon, see ADR-001 boundary discussion

## Protobuf schema

The wire protocol is defined in [`proto/ward.proto`](../proto/ward.proto)
and released under CC0 1.0 (public domain). The `.proto` file is the
single source of truth for the API. All SDKs are generated from it.
