// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! Protobuf types and tonic gRPC stubs for the ward sandbox daemon's
//! public wire protocol.
//!
//! This crate exists so that ward's AGPL-3.0 server-side workspace
//! (`ward-core`, `ward-daemon`, `ward-runtime`, `ward-mcp`) and its
//! Apache-2.0 SDK side (`sdks/rust/ward-client`, downstream consumers)
//! can share a single set of generated bindings without crossing the
//! license boundary. See:
//!
//! - [ADR-004](https://github.com/igorjs/ward/blob/main/docs/adr/004-cli-protocol.md):
//!   the protobuf schema itself is CC0.
//! - [ADR-017](https://github.com/igorjs/ward/blob/main/docs/adr/017-license-posture.md):
//!   why the SDK and server boundary lives at the proto layer.
//!
//! ## Usage
//!
//! ```ignore
//! use ward_proto::pb;
//!
//! let req = pb::CreateSandboxRequest {
//!     image: "alpine".into(),
//!     ..Default::default()
//! };
//! ```
//!
//! Server-side workspace crates re-export `pb` so call sites stay
//! `crate::pb::...`-shaped; see `ward-core/src/lib.rs`.

/// Generated protobuf messages, enums, and the tonic gRPC service +
/// client stubs for the `ward.v1` package.
pub mod pb {
    tonic::include_proto!("ward.v1");
}
