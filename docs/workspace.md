# Workspace layout

```
ward-core/     Library crate: protocol types, Backend trait, libkrun FFI,
               SandboxManager, broker, image pull/unpack.
ward-daemon/   wardd binary: gRPC server over Unix socket, hosts the manager.
ward-cli/      ward binary: thin CLI client over the same gRPC.
ward-agent/    Guest-side init binary + vsock RPC protocol (boot integration: #9).
ward-proto/    Protobuf types + tonic gRPC stubs for the daemon's public wire
               protocol; Apache-2.0 boundary crate shared by the AGPL
               workspace and the SDKs.
ward-runtime/  Embedded runtime: boots libkrun-backed sandboxes in-process,
               no daemon required.
ward-mcp/      Model Context Protocol server: exposes sandboxed-execution
               tools to LLM agents over stdio JSON-RPC.
ward-net/      Network backends: passt (default rootless), gvproxy, and a
               smoltcp research path.
proto/         ward.proto, ward_agent.proto. Single source of truth for the wire.
sdks/          Apache-2.0 client libraries (Python, TypeScript, Go, Rust).
vendor/        Pinned libkrun version + bottle checksums.
docs/          ADRs and SPEC.md (table of contents).
scripts/       Maintenance helpers (e.g. diff-libkrun.sh).
```
