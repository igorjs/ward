// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-core build script.
//!
//! - Always: compile `proto/ward.proto` into Rust types.
//! - When `--features krunvm` is enabled: emit linker directives for
//!   `libkrun` and `libkrunfw`. FFI symbol declarations live in
//!   `src/backend/krun_ffi.rs` (hand-maintained, no bindgen, no
//!   `krun-sys` crate). The linker uses the system's default search
//!   path: developers install the libraries once via `brew install
//!   slp/krun/libkrun slp/krun/libkrunfw` (macOS) or distro packages
//!   (Linux). See DEVELOPMENT.md for setup details.
//!
//! Why not vendor at build time? End-user release artefacts ship the
//! libraries pre-built via the separate `igorjs/ward-vendor` repo
//! (public) and bundled by `release.yml` in this repo (under
//! `--features krunvm`). End users download a single self-contained
//! artefact; devs install libkrun via their package manager once.
//! (An earlier in-tree download approach via build.rs was reverted in
//! commit 5218cb6 because cargo runs dependency build scripts before
//! dependents — the obstacle no longer applies now that we own the
//! FFI surface directly, but the end-user UX still favours prebuilt
//! distribution over per-build downloads.)

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward.proto"], &["../proto"])?;

    #[cfg(feature = "krunvm")]
    {
        println!("cargo:rustc-link-lib=krun");
        println!("cargo:rustc-link-lib=krunfw");
    }

    Ok(())
}
