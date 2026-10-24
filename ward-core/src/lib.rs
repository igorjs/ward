// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

pub use ward_archive as archive;
pub mod backend;
pub mod comms;
pub mod config;
pub mod egress;
pub mod grpc;
pub mod protocol;
pub mod sandbox;
pub mod utf8;
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

/// Turn this process into a microVM host when ward started it as one.
///
/// Every binary that can run sandboxes must call this first thing in
/// `main`, before any other setup. It returns immediately in a normal run
/// and never returns in a VM host process. Without the `krunvm` feature
/// it does nothing.
pub fn run_vm_shim_if_requested() {
    #[cfg(feature = "krunvm")]
    backend::vmshim::run_if_requested();
}
