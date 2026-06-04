# ADR-005: SDK Strategy

**Status:** Accepted — SDK distribution model amended by [ADR-016](016-embedded-mode-microvms.md) (2026-06-04)
**Date:** 2026-05-12
**Authors:** Igor

## Context

Ward needs SDKs in multiple languages. With gRPC + protobuf as the protocol (ADR-004), SDK creation becomes primarily a code generation task rather than a hand-writing task.

## Decision

### SDK architecture: generated clients + idiomatic wrappers

Each SDK consists of:

1. **Generated gRPC client** from `ward.proto` using the language's standard protobuf/gRPC toolchain. This handles serialization, deserialization, streaming, connection management, and error propagation.

2. **Idiomatic wrapper** (~200–500 lines, hand-written) that makes the generated client feel native to the language. This maps gRPC patterns to language conventions: `async/await` in TypeScript/Python, channels in Go, `Result` in Rust, blocks in Ruby.

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

### SDK repository structure

```
github.com/igorjs/ward           -- daemon + proto (Rust, AGPL-3.0)
github.com/igorjs/ward-sdk-ts    -- TypeScript/Deno + Node.js SDK (Apache 2.0)
github.com/igorjs/ward-sdk-py    -- Python SDK (Apache 2.0)
github.com/igorjs/ward-sdk-go    -- Go SDK (Apache 2.0)
github.com/igorjs/ward-sdk-rs    -- Rust SDK (Apache 2.0)
github.com/igorjs/ward-sdk-rb    -- Ruby SDK (Apache 2.0)
github.com/igorjs/ward-sdk-jvm   -- Java/Kotlin SDK (Apache 2.0)
```

Each SDK repo contains:

1. A git submodule pointing at the `ward` repo's `proto/` directory (or a CI-mirrored copy)
2. Generated gRPC client code (committed, not `.gitignore`d, so users don't need protoc)
3. The idiomatic wrapper
4. Tests

### Build pipeline

A CI workflow in `igorjs/ward` regenerates client code for all SDK languages whenever `proto/ward.proto` changes, and opens PRs against each SDK repo with the updated generated code.

### Protocol specification

The `.proto` file at `proto/ward.proto` is the source of truth. It is released under CC0 1.0 (public domain) so third parties can generate their own clients without any license obligations.

### Current state

No SDKs have been implemented yet. The daemon ships with the `ward` CLI (in `ward-cli/`), which serves as the reference gRPC consumer. SDK work begins after v0.1.0 of the daemon ships.

## Consequences

- SDK creation effort is ~200-500 lines of wrapper code per language, not ~1000 lines of hand-written serialization.
- Schema changes propagate automatically to all SDKs via generated code.
- Third parties can generate clients in any gRPC-supported language from the `.proto` file.
- The `.proto` file must be maintained carefully: field numbers cannot be reused, fields cannot change type.

## Amendment (ADR-016, 2026-06-04)

ADR-016 introduces an **embedded mode** alongside the daemon. SDKs now ship in two
flavours per language:

- **Embedded path:** links `ward-runtime` (Rust) or a small native helper (Python /
  TS / Go) so `Sandbox::builder(...).create()` works with no daemon install.
- **Daemon path:** generated gRPC client per this ADR — used when the SDK is
  configured with a daemon address.

The protobuf surface and SDK tiers in this ADR remain authoritative for the
daemon path. See ADR-016 for the embedded-path rationale and implementation
order.
