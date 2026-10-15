// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-core build script.
//!
//! - Always: compile `proto/ward_agent.proto` into Rust types. The
//!   public service surface (`proto/ward.proto`) now lives in the
//!   standalone `ward-proto` crate so the AGPL workspace and the
//!   Apache-2.0 SDK can share one codegen output (see ADR-017 and
//!   the `ward-proto` crate). `ward_agent.proto` stays here because
//!   it is the in-VM agent protocol consumed only by AGPL crates
//!   (`ward-core` ↔ `ward-agent`).
//! - libkrun is not linked here: with `--features krunvm` it is embedded by
//!   `ward-bundle` and loaded at runtime (`src/backend/krun_ffi.rs`).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ward.proto moved to the standalone `ward-proto` crate (Apache-2.0).
    // ward-core now compiles only the in-VM agent protocol.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward_agent.proto"], &["../proto"])?;

    Ok(())
}
