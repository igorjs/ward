# ADR-017: License Posture for Embedded SDK Distribution

**Status:** Proposed
**Date:** 2026-06-04
**Authors:** Igor

## Context

[ADR-016](016-embedded-mode-microvms.md) considered making the SDK
embed `ward-runtime` directly so applications could
`cargo add ward-sdk` and run microVMs in-process. The implementation
hit a hard license boundary:

- `ward-core` and `ward-runtime` are **AGPL-3.0-only** (workspace
  default).
- `sdks/rust/ward-client` is **Apache-2.0** *by design* so library
  users aren't infected by the workspace license. The crate's
  `Cargo.toml` carries an explicit comment forbidding any
  `path =` dependency on AGPL crates.

A direct `path` dep from the SDK to `ward-runtime` would silently
relicense the SDK. ADR-016's embedded SDK story is parked behind this
decision. ward-mcp escaped the constraint by being a server binary
(it links `ward-runtime` and stays AGPL); the SDK does not have that
escape hatch.

Three options were enumerated in ADR-016's amendment. This ADR picks
one as the v0.1 default and leaves the door open to revisit.

## Options

### A. Helper-binary embedded mode

The SDK ships in two halves:

- `ward-sdk` (Apache-2.0, gRPC client). Detects whether a daemon is
  reachable.
- `ward-embed-helper` (AGPL, links `ward-runtime`). A small binary the
  SDK spawns on demand. SDK and helper communicate over a
  socket-pair or stdio gRPC.

To the SDK user, `Sandbox::builder().create()` "just works" with no
daemon install — the helper is shipped alongside the SDK in the
distribution package, and the AGPL boundary is the SDK ↔ helper
process boundary.

**Pros:**
- Microsandbox-style "no infrastructure" ergonomics for embedded SDK
  users, *without* relicensing.
- Helper can be reused for one-shot CLI flows (`ward run --embedded`).

**Cons:**
- Two artefacts to package and distribute per language SDK.
- Cross-platform binary distribution via `pip` / `npm` / `cargo` is
  real work (target triples, signed binaries, etc.).
- Process-spawn cost on every SDK init (small, but real on cold
  paths).

### B. `ward-proto` crate + SDK stays gRPC-only

Extract the protobuf-generated types into a standalone `ward-proto`
crate published under Apache-2.0 (or CC0, matching the `.proto`
file's license). SDKs depend on `ward-proto` for the wire types and
ship as gRPC-only clients. Embedded mode is **not offered** in the
SDK; users who want embedded use the daemon, the CLI, or `ward-mcp`.

**Pros:**
- Zero relicensing.
- Already the de facto current state — `ward-client/build.rs` generates
  its own protobuf types from `proto/ward.proto`. Codifying that as
  `ward-proto` is mechanical.
- The license boundary is honest: "want embedded? use the AGPL
  runtime directly. Want a thin client? use the SDK."
- Aligns with how Firecracker / containerd / Kubernetes split client
  SDKs from server runtimes.

**Cons:**
- Ward gives up the marketing point of "embedded SDK ergonomics."
  Users comparing ward to microsandbox see a strictly larger setup
  cost for the SDK path.
- `ward-mcp` becomes the only "no daemon required" surface ward
  offers. That's defensible (MCP is the agent-integration story) but
  narrows the SDK use case to fleet operators.

### C. Relicense `ward-core` / `ward-runtime` to Apache-2.0 or MIT

Drop AGPL on the runtime. The daemon (`wardd`) could keep AGPL or
also be relicensed. SDK can then `path =` to runtime cleanly.

**Pros:**
- Removes the boundary entirely.
- Maximises adoption — AGPL is a known adoption blocker for many
  corporate users even when their use is allowed by the license.
- Matches what successful adjacent projects (microsandbox,
  Firecracker, containerd, runc) do.

**Cons:**
- One-way decision. Once Apache-2.0 is in v0.1.0, ward can't take it
  back.
- Gives up AGPL's network-copyleft enforcement, which was the
  original reason to pick it (preventing closed-source SaaS forks
  from siphoning improvements).
- Requires a contributor agreement / CLA audit. The current
  CONTRIBUTING.md uses DCO + a permissive CLA; that's compatible with
  relicensing-by-author but should be confirmed explicitly.
- A real argument for AGPL's value here is hard to make: ward isn't a
  hosted service, its core IP is composed of well-known parts
  (libkrun + tonic + a manager), and the projects ward most directly
  competes with are permissively licensed. AGPL produces friction
  without producing the network-copyleft moat it's designed for.

## Decision

**Adopt Option B for v0.1.0.** Codify the SDK ↔ runtime separation by:

1. Extracting `proto/ward.proto` compile output into a standalone
   `ward-proto` crate (Apache-2.0). Both `ward-client` and `ward-core`
   depend on it for the wire types; no duplicated codegen.
2. Documenting the SDK's intended scope as "thin gRPC client for the
   daemon" — full stop. Embedded mode is not the SDK's product.
3. Pointing users who want embedded ergonomics at `ward-mcp`
   (server binary) or, in the future, an Option-A helper binary if a
   compelling embedded-SDK use case appears that MCP doesn't already
   cover.

**Schedule Option C as a recurring topic.** Revisit when:

- A non-trivial corporate user surfaces wanting to embed ward in a
  proprietary product. AGPL friction is concrete at that point.
- The maintainer's read of the project's purpose shifts from "personal
  / opinionated runtime" toward "infrastructure that wants broad
  adoption."

A future ADR-018 (or this one re-opened) can flip to Option C if
either trigger fires. The decision is reversible *until* an outside
contributor lands non-trivial code under AGPL terms — at that point
the calculus changes (need their consent to relicense) and Option C
becomes meaningfully more expensive.

## Consequences

### Positive

- v0.1.0 ships without a license overhang or pending tech-debt PR.
- `ward-proto` extraction is mechanical work that benefits both
  workspace internals and SDKs.
- The honesty win: stop selling "SDK embedded mode" as a near-term
  feature when it isn't.

### Negative

- No literal answer to "but microsandbox has an embedded SDK." The
  marketing story for ward becomes "use the daemon, or use the MCP
  server, or use the SDK against a daemon" — three real paths but
  none that's a 1:1 microsandbox replacement.
- Embedded SDK becomes a deliberate non-goal of the SDK. A future
  reversal (helper binary or relicense) is a real undertaking, not a
  quiet flip.

### Neutral

- ward-runtime remains AGPL. ward-mcp remains AGPL. The CLI remains
  AGPL. None of those are libraries downstream consumers link
  against, so the AGPL boundary stays on the natural process line.

## Implementation

1. Create `ward-proto` crate (Apache-2.0, edition matches workspace).
2. Move `tonic_prost_build` codegen for `ward.proto` out of
   `ward-client/build.rs` and `ward-core/build.rs` into `ward-proto`.
3. Both `ward-client` and `ward-core` depend on `ward-proto` for the
   `pb` module.
4. Update `sdks/rust/ward-client/Cargo.toml` license comment to refer
   to this ADR.
5. Update README and `docs/positioning.md` to reflect the explicit
   SDK scope.

Effort: ~half a day of mechanical refactor. Reversible if Option C
later fires.

### Implementation log

Landed 2026-06-25 (#94). What actually shipped:

1. `ward-proto/` (Apache-2.0) — workspace member, `build.rs` compiles
   `../proto/ward.proto` via `tonic_prost_build`, `src/lib.rs` exposes
   `pub mod pb`.
2. `ward-core` re-exports it as `pub use ward_proto::pb;` so all
   server-side call sites keep working unchanged
   (`crate::pb::*` resolves to `ward_proto::pb::*`).
3. `ward-core/build.rs` no longer compiles `ward.proto`. It still
   compiles `ward_agent.proto` — that protocol stays AGPL-internal
   because the only consumers are `ward-core` ↔ `ward-agent`.
4. `sdks/rust/ward-client` drops its own `build.rs` and
   `tonic-prost-build` build-dep; depends on `ward-proto` instead and
   re-exports `pb` identically. Downstream API shape unchanged.
5. License boundary verified post-merge:
   `cargo tree -p ward-client --edges=no-dev` shows no path-dep on
   any AGPL workspace crate (only `ward-proto`).

## References

- ADR-005: SDK strategy (amended by this ADR's scope clarification)
- ADR-016: embedded-mode microVMs (this ADR closes the open question
  it raised about SDK embedded mode)
- [microsandbox](https://github.com/superradcompany/microsandbox) for
  the Apache-2.0 embedded-SDK posture this ADR considers and declines
  for v0.1.
