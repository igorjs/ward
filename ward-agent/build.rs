// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Compile the internal guest-agent protocol into Rust types.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    prost_build::compile_protos(&["../proto/ward_agent.proto"], &["../proto"])?;
    Ok(())
}
