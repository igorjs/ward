# Workspace layout

```
ward-core/     Library crate: protocol types, Backend trait, libkrun FFI,
               SandboxManager, broker, image pull/unpack.
ward-daemon/   wardd binary: gRPC server over Unix socket, hosts the manager.
ward-cli/      ward binary: thin CLI client over the same gRPC.
ward-agent/    Guest-side init binary + vsock RPC protocol (boot integration: #9).
proto/         ward.proto, ward_agent.proto. Single source of truth for the wire.
sdks/          Apache-2.0 client libraries (Python, TypeScript, Go, Rust).
vendor/        Pinned libkrun version + bottle checksums.
docs/          ADRs and SPEC.md (table of contents).
scripts/       Maintenance helpers (e.g. diff-libkrun.sh).
```
