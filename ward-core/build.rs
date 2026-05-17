// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-core build script.
//!
//! - Always: compile `proto/ward.proto` into Rust types.
//! - When `--features krunvm` is enabled: emit linker directives for
//!   `libkrun` and `libkrunfw`. Discovery is left to the standard pkg-config
//!   pipeline that `krun-sys`'s own build.rs already uses — that means
//!   developers install the libraries once via `brew install slp/krun/libkrun
//!   slp/krun/libkrunfw` (macOS) or distro packages (Linux). See
//!   DEVELOPMENT.md for setup details.
//!
//! Why not vendor at build time? An earlier attempt (commit 40656e0) wired
//! ward-core/build.rs to download pre-built libkrun bottles from GitHub
//! Releases. It didn't work because cargo runs dependency build scripts
//! BEFORE dependents — `krun-sys` is a transitive dep of ward-core, so its
//! build.rs fires before ours, meaning we can't prepare `PKG_CONFIG_PATH`
//! for it. The right fix is either a forked `ward-krun-sys` crate (heavy)
//! or shifting the "no install required" promise to *end users* via
//! prebuilt binary distribution (light). We chose the latter.
//!
//! Bottles for end-user release artefacts are produced by the separate
//! `igorjs/ward-vendor` repo (public) and consumed by `release.yml` in
//! this repo (under `--features krunvm` bundling mode). End users
//! download a single self-contained artefact; devs install libkrun via
//! their package manager once.

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
