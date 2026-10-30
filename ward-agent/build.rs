// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Compile the internal guest-agent protocol into Rust types.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    prost_build::Config::new()
        // Hand-written in lib.rs so the content never reaches a log line.
        .skip_debug(["ward.agent.v1.SecretFile"])
        .compile_protos(&["../proto/ward_agent.proto"], &["../proto"])?;
    Ok(())
}
