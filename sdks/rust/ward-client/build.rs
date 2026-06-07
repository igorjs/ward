// Copyright 2026 Ward Contributors. SPDX-License-Identifier: Apache-2.0

//! ward-client build script.
//!
//! Generates Rust types from `proto/ward.proto` directly inside this
//! crate so that ward-client stays self-contained at the Cargo level.
//! The `.proto` file is CC0-licensed (per ADR-004); compiling it here
//! produces Apache-2.0-clean generated code with no AGPL linkage to
//! ward-core. See the license comment in `Cargo.toml` for why this
//! matters.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = std::path::PathBuf::from("../../../proto");
    let proto_file = proto_root.join("ward.proto");

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&[&proto_file], &[&proto_root])?;

    // Rerun codegen if the proto changes. Cargo's default heuristic for
    // build.rs is "rerun if build.rs itself changes"; without these, an
    // edit to ward.proto wouldn't trigger regeneration in CI.
    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed={}", proto_root.display());

    Ok(())
}
