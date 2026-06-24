// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! ward-proto build script.
//!
//! Compiles `proto/ward.proto` (CC0, per ADR-004) into Apache-2.0
//! generated Rust. Both the AGPL workspace crates (ward-core etc.) and
//! the Apache-2.0 SDK crates (sdks/rust/ward-client) consume the output
//! through `pub mod pb` in lib.rs — no crate has to run codegen twice,
//! and the SDK ↔ AGPL boundary stays at this layer.
//!
//! `ward_agent.proto` is deliberately NOT compiled here: it is the
//! agent-side vsock protocol and lives entirely inside the AGPL
//! workspace (ward-core, ward-agent). Mixing it into ward-proto would
//! pull AGPL-only callers into the SDK boundary needlessly.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = std::path::PathBuf::from("../proto");
    let proto_file = proto_root.join("ward.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[&proto_file], &[&proto_root])?;

    // Rerun codegen if the proto changes. Cargo's default heuristic
    // for build.rs is "rerun if build.rs itself changes"; without
    // these, an edit to ward.proto would not trigger regeneration.
    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed={}", proto_root.display());

    Ok(())
}
