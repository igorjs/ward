// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-core build script.
//!
//! Two responsibilities:
//!
//! 1. Compile `proto/ward.proto` into Rust types (always).
//! 2. When `--features krunvm` is enabled: download the matching
//!    pre-built libkrun tarball from the project's GitHub Releases,
//!    verify its SHA-256, extract under `OUT_DIR`, and configure
//!    pkg-config so the downstream `krun-sys` build links against
//!    our vendored copy instead of a system install.
//!
//! Goal: `cargo build --features krunvm` works on a fresh clone with
//! no `brew install` or `apt-get install` step. The libkrun dependency
//! lives inside ward's release cadence, not the user's OS package
//! manager.
//!
//! Caching: the downloaded tarball lives under `OUT_DIR/libkrun-cache/`,
//! so incremental builds reuse it. Re-download triggers only when
//! `vendor/libkrun-build/version.txt` changes (rerun-if-changed).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // ----- proto compilation (always) --------------------------------
    println!("cargo:rerun-if-changed=../proto/ward.proto");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward.proto"], &["../proto"])
        .expect("compile ward.proto");

    // ----- libkrun vendoring (only when feature enabled) -------------
    println!("cargo:rerun-if-changed=../vendor/libkrun-build/version.txt");
    println!("cargo:rerun-if-changed=../vendor/libkrun-build/checksums.txt");

    if env::var("CARGO_FEATURE_KRUNVM").is_err() {
        // Default builds skip the krunvm feature; the stub backend in
        // KrunvmBackend covers tests and typical development with no
        // system libkrun present.
        return;
    }

    if let Err(e) = vendor_libkrun() {
        // First-run failures are usually "no GitHub Release for this
        // version yet" or network issues. Loud panic so the user sees
        // a clear actionable message.
        panic!("libkrun vendoring failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// libkrun download + extract + configure
// ---------------------------------------------------------------------------

fn vendor_libkrun() -> Result<(), String> {
    let target = env::var("TARGET").map_err(|e| format!("TARGET env unset: {e}"))?;
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| format!("OUT_DIR unset: {e}"))?);

    let version = read_version()?;
    let tarball_name = format!("libkrun-{version}-{target}.tar.gz");
    let expected_sha = lookup_checksum(&tarball_name)?;

    let cache_dir = out_dir.join("libkrun-cache");
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("create cache dir {}: {e}", cache_dir.display()))?;
    let tarball_path = cache_dir.join(&tarball_name);

    if !tarball_path.exists() {
        let url = format!(
            "https://github.com/igorjs/ward/releases/download/libkrun-v{version}/{tarball_name}"
        );
        download(&url, &tarball_path)?;
    }

    verify_sha256(&tarball_path, &expected_sha)?;

    let extract_dir = out_dir.join("libkrun").join(&target);
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir)
            .map_err(|e| format!("remove stale extract dir: {e}"))?;
    }
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("mkdir extract dir: {e}"))?;
    extract(&tarball_path, &extract_dir)?;

    rewrite_pkgconfig(&extract_dir)?;
    configure_link(&extract_dir, &target);

    Ok(())
}

/// Read the pinned version from `vendor/libkrun-build/version.txt`.
fn read_version() -> Result<String, String> {
    let path = Path::new("../vendor/libkrun-build/version.txt");
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err("version.txt is empty".into());
    }
    Ok(trimmed)
}

/// Look up the expected SHA-256 for `tarball_name` in
/// `vendor/libkrun-build/checksums.txt`. Format is `<hex>  <filename>`,
/// one per line; lines starting with `#` are ignored.
fn lookup_checksum(tarball_name: &str) -> Result<String, String> {
    let path = Path::new("../vendor/libkrun-build/checksums.txt");
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let sha = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        if name == tarball_name {
            return Ok(sha.to_string());
        }
    }
    Err(format!(
        "no SHA-256 entry for '{tarball_name}' in checksums.txt — \
         has the vendor-libkrun workflow run for this version + target? \
         Trigger it from the Actions tab, then paste the resulting hash \
         into vendor/libkrun-build/checksums.txt and commit."
    ))
}

/// Download `url` to `dest` via curl. Shelled out instead of pulling a
/// TLS-capable HTTP crate into build-deps; curl is on every supported host.
fn download(url: &str, dest: &Path) -> Result<(), String> {
    println!("cargo:warning=downloading {url}");
    let status = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--retry",
            "3",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .map_err(|e| format!("spawn curl: {e}"))?;
    if !status.success() {
        return Err(format!("curl failed for {url} (exit {status})"));
    }
    Ok(())
}

/// Verify `path`'s SHA-256 matches `expected_hex`. Shells out to
/// `sha256sum` (Linux) or `shasum -a 256` (macOS).
fn verify_sha256(path: &Path, expected_hex: &str) -> Result<(), String> {
    let output = if cfg!(target_os = "macos") {
        Command::new("shasum")
            .arg("-a")
            .arg("256")
            .arg(path)
            .output()
    } else {
        Command::new("sha256sum").arg(path).output()
    }
    .map_err(|e| format!("spawn hash tool: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "hash tool failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let actual_hex = stdout.split_whitespace().next().unwrap_or("");
    if actual_hex != expected_hex {
        return Err(format!(
            "SHA-256 mismatch for {}: expected {expected_hex}, got {actual_hex}",
            path.display()
        ));
    }
    Ok(())
}

/// Extract the tarball into `dest`. Uses the system `tar` (faster than
/// pure-Rust tar+flate2 at build time, and ubiquitous on macOS+Linux).
fn extract(tarball: &Path, dest: &Path) -> Result<(), String> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest)
        .status()
        .map_err(|e| format!("spawn tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar -xzf failed (exit {status})"));
    }
    Ok(())
}

/// The tarball ships a libkrun.pc with `prefix=__VENDOR_PREFIX__` as a
/// placeholder. Rewrite it to the absolute extracted path so pkg-config's
/// downstream consumers (krun-sys's build.rs) get correct `-L`/`-I` flags.
fn rewrite_pkgconfig(extract_dir: &Path) -> Result<(), String> {
    let pc_path = extract_dir.join("lib/pkgconfig/libkrun.pc");
    let raw = std::fs::read_to_string(&pc_path)
        .map_err(|e| format!("read {}: {e}", pc_path.display()))?;
    let prefix = extract_dir
        .to_str()
        .ok_or_else(|| "extract_dir path is not UTF-8".to_string())?;
    let rewritten = raw.replace("__VENDOR_PREFIX__", prefix);
    std::fs::write(&pc_path, rewritten).map_err(|e| format!("write {}: {e}", pc_path.display()))?;
    Ok(())
}

/// Configure the downstream krun-sys + final binary build:
///   - PKG_CONFIG_PATH points krun-sys's bindgen + linker probe at our
///     rewritten libkrun.pc.
///   - rustc-link-search makes the linker find libkrun.{dylib,so}.
///   - rustc-link-arg-bins sets the binary's runtime search path so it
///     loads our dylib from a sibling `lib/` directory at run time.
fn configure_link(extract_dir: &Path, target: &str) {
    let lib_dir = extract_dir.join("lib");
    let pc_dir = lib_dir.join("pkgconfig");

    // PKG_CONFIG_PATH must be set in the process env (not via rustc-env,
    // which only affects rustc invocations). krun-sys's build.rs runs
    // AFTER ours in cargo's dep-ordering pass, so this propagates.
    //
    // SAFETY: env::set_var is sound here because cargo invokes build
    // scripts single-threaded, one per crate at a time.
    unsafe {
        env::set_var("PKG_CONFIG_PATH", &pc_dir);
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    // Runtime rpath: where the binary looks for the dylib at load time.
    // We tell it "next to the executable, and one dir up under lib/" so
    // packaging can place dylibs in either layout.
    if target.contains("apple") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path");
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/../lib");
    } else {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib");
    }
}
