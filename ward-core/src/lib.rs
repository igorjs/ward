// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub mod backend;
pub mod comms;
pub mod config;
pub mod egress;
pub mod grpc;
pub mod protocol;
pub mod sandbox;
pub mod validate;
pub mod volume;

/// Generated protobuf types and gRPC service traits.
///
/// Re-exported from the standalone `ward-proto` crate so the AGPL
/// workspace (this crate + ward-daemon + ward-runtime + ward-mcp) and
/// the Apache-2.0 SDK side (`sdks/rust/ward-client`) share a single
/// codegen output without crossing the license boundary. The wire
/// types stay reachable through the historical `crate::pb::*` path so
/// existing call sites do not move. See ADR-017 for the rationale and
/// the `ward-proto` crate docs for the boundary contract.
pub use ward_proto::pb;
