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
//! - When `--features krunvm` is enabled: emit linker directives for
//!   `libkrun` and `libkrunfw`. FFI symbol declarations live in
//!   `src/backend/krun_ffi.rs` (hand-maintained, no bindgen, no
//!   `krun-sys` crate). The linker uses the system's default search
//!   path: developers install the libraries once via `brew install
//!   slp/krun/libkrun slp/krun/libkrunfw` (macOS) or distro packages
//!   (Linux). See DEVELOPMENT.md for setup details.
//!
//! Why not vendor at build time? End-user release artefacts ship the
//! libraries pre-built via the separate `igorjs/libkrun-builds` repo
//! (public) and bundled by `release.yml` in this repo (under
//! `--features krunvm`). End users download a single self-contained
//! artefact; devs install libkrun via their package manager once.
//! (An earlier in-tree download approach via build.rs was reverted in
//! commit 5218cb6 because cargo runs dependency build scripts before
//! dependents — the obstacle no longer applies now that we own the
//! FFI surface directly, but the end-user UX still favours prebuilt
//! distribution over per-build downloads.)

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ward.proto moved to the standalone `ward-proto` crate (Apache-2.0).
    // ward-core now compiles only the in-VM agent protocol.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward_agent.proto"], &["../proto"])?;

    #[cfg(feature = "krunvm")]
    {
        // Release/CI builds extract the libkrun bottle to a custom prefix
        // and export LIBKRUN_PREFIX. Point the linker at <prefix>/lib so
        // -lkrun/-lkrunfw resolve. Local dev installs (brew, distro pkg)
        // land on the default search path, so the env var is optional.
        println!("cargo:rerun-if-env-changed=LIBKRUN_PREFIX");
        if let Ok(prefix) = std::env::var("LIBKRUN_PREFIX") {
            println!("cargo:rustc-link-search=native={prefix}/lib");
        }
        println!("cargo:rustc-link-lib=krun");
        println!("cargo:rustc-link-lib=krunfw");
    }

    Ok(())
}
