// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Prepares everything a ward binary needs to run microVMs without any
//! system library:
//!
//! - the pinned libkrun + libkrunfw bottle from `igorjs/libkrun-builds`
//!   (verified against `vendor/libkrun-checksums.txt`),
//! - the `ward-agent` guest binary, cross-compiled to a static Linux
//!   musl executable with the `rust-lld` linker that ships with rustup.
//!
//! The payloads are gzip-compressed into `OUT_DIR` and embedded by
//! `src/lib.rs`. Without the `embed` feature the payloads are empty, so a
//! plain `cargo build` of the stub backend downloads nothing.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

const RELEASE_BASE: &str = "https://github.com/igorjs/libkrun-builds/releases/download";
const TARBALL_OVERRIDE_ENV: &str = "WARD_LIBKRUN_TARBALL";
const SKIP_AGENT_ENV: &str = "WARD_BUNDLE_SKIP_AGENT";
const BUNDLE_ID_HEX_LEN: usize = 16;

/// Mach-O constants used to rewrite libkrunfw's install name.
const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_ID_DYLIB: u32 = 0xd;
const MACHO_HEADER_LEN: usize = 32;
const MACHO_NCMDS_OFFSET: usize = 16;
const DYLIB_NAME_FIELD_OFFSET: usize = 8;
/// libkrun asks dyld for this exact leaf name when it loads its kernel.
const KRUNFW_MACOS_INSTALL_NAME: &str = "libkrunfw.5.dylib";

struct Layout {
    /// Prefix of the libkrun file inside the tarball's `lib/` directory.
    krun_prefix: &'static str,
    /// Prefix of the libkrunfw file inside the tarball's `lib/` directory.
    krunfw_prefix: &'static str,
    /// File names ward extracts to at runtime.
    krun_runtime_name: &'static str,
    krunfw_runtime_name: &'static str,
    guest_target: &'static str,
    macos: bool,
}

fn layout_for(target: &str) -> Option<Layout> {
    match target {
        "aarch64-apple-darwin" => Some(Layout {
            krun_prefix: "libkrun.",
            krunfw_prefix: "libkrunfw.",
            krun_runtime_name: "libkrun.1.dylib",
            krunfw_runtime_name: "libkrunfw.5.dylib",
            guest_target: "aarch64-unknown-linux-musl",
            macos: true,
        }),
        "x86_64-unknown-linux-gnu" => Some(Layout {
            krun_prefix: "libkrun.so.",
            krunfw_prefix: "libkrunfw.so.",
            krun_runtime_name: "libkrun.so.1",
            krunfw_runtime_name: "libkrunfw.so.5",
            guest_target: "x86_64-unknown-linux-musl",
            macos: false,
        }),
        "aarch64-unknown-linux-gnu" => Some(Layout {
            krun_prefix: "libkrun.so.",
            krunfw_prefix: "libkrunfw.so.",
            krun_runtime_name: "libkrun.so.1",
            krunfw_runtime_name: "libkrunfw.so.5",
            guest_target: "aarch64-unknown-linux-musl",
            macos: false,
        }),
        _ => None,
    }
}

type BuildResult<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> BuildResult<()> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let workspace = manifest_dir
        .parent()
        .ok_or("ward-bundle has no parent directory")?
        .to_path_buf();
    let target = env::var("TARGET")?;

    for path in [
        "build.rs",
        "../vendor/libkrun-version.txt",
        "../vendor/libkrun-checksums.txt",
        "../ward-agent/src",
        "../ward-agent/Cargo.toml",
        "../proto/ward_agent.proto",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed={TARBALL_OVERRIDE_ENV}");
    println!("cargo:rerun-if-env-changed={SKIP_AGENT_ENV}");

    let embed = env::var_os("CARGO_FEATURE_EMBED").is_some();
    let layout = layout_for(&target);

    let (supported, krun, krunfw, agent) = match (embed, &layout) {
        (true, Some(layout)) => {
            let (krun, krunfw) = fetch_libs(&workspace, &out_dir, &target, layout)?;
            let agent = if env::var_os(SKIP_AGENT_ENV).is_some() {
                Vec::new()
            } else {
                build_agent(&workspace, &out_dir, layout.guest_target)?
            };
            (true, krun, krunfw, agent)
        }
        (true, None) => {
            println!(
                "cargo:warning=ward-bundle: no libkrun bottle for target {target}; \
                 real microVMs are unavailable on this target"
            );
            (false, Vec::new(), Vec::new(), Vec::new())
        }
        (false, _) => (false, Vec::new(), Vec::new(), Vec::new()),
    };

    let mut hasher = Sha256::new();
    hasher.update(&krun);
    hasher.update(&krunfw);
    hasher.update(&agent);
    let digest = hex(&hasher.finalize());
    let bundle_id = &digest[..BUNDLE_ID_HEX_LEN];

    write_gz(&out_dir.join("libkrun.gz"), &krun)?;
    write_gz(&out_dir.join("libkrunfw.gz"), &krunfw)?;
    write_gz(&out_dir.join("ward-agent.gz"), &agent)?;

    let (krun_name, krunfw_name) = layout
        .as_ref()
        .map_or(("", ""), |l| (l.krun_runtime_name, l.krunfw_runtime_name));
    fs::write(
        out_dir.join("bundle_consts.rs"),
        format!(
            "pub const SUPPORTED: bool = {supported};\n\
             pub const BUNDLE_ID: &str = \"{bundle_id}\";\n\
             pub const KRUN_FILE_NAME: &str = \"{krun_name}\";\n\
             pub const KRUNFW_FILE_NAME: &str = \"{krunfw_name}\";\n\
             pub const HAS_AGENT: bool = {};\n",
            !agent.is_empty()
        ),
    )?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_gz(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut enc = GzEncoder::new(fs::File::create(path)?, Compression::best());
    enc.write_all(data)?;
    enc.finish()?;
    Ok(())
}

fn fetch_libs(
    workspace: &Path,
    out_dir: &Path,
    target: &str,
    layout: &Layout,
) -> BuildResult<(Vec<u8>, Vec<u8>)> {
    let version = fs::read_to_string(workspace.join("vendor/libkrun-version.txt"))?
        .trim()
        .to_string();
    let tarball_name = format!("libkrun-{version}-{target}.tar.gz");
    let expected = expected_checksum(workspace, &tarball_name)?;
    let tarball = locate_tarball(out_dir, &version, &tarball_name, &expected)?;

    let mut krun = None;
    let mut krunfw = None;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(&tarball)?));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        if path.parent().and_then(Path::file_name) != Some("lib".as_ref()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        // "libkrunfw." must be tested before "libkrun." only on macOS naming,
        // where "libkrun." never matches "libkrunfw.5.dylib".
        let mut bytes = Vec::new();
        if name.starts_with(layout.krunfw_prefix) {
            entry.read_to_end(&mut bytes)?;
            krunfw = Some(bytes);
        } else if name.starts_with(layout.krun_prefix) {
            entry.read_to_end(&mut bytes)?;
            krun = Some(bytes);
        }
    }
    let krun = krun.ok_or("libkrun library missing from bottle")?;
    let mut krunfw = krunfw.ok_or("libkrunfw library missing from bottle")?;

    if layout.macos {
        krunfw = rename_macos_dylib(out_dir, &krunfw)?;
    }
    Ok((krun, krunfw))
}

fn expected_checksum(workspace: &Path, tarball_name: &str) -> BuildResult<String> {
    let sums = fs::read_to_string(workspace.join("vendor/libkrun-checksums.txt"))?;
    sums.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| {
            let mut parts = l.split_whitespace();
            let sum = parts.next()?;
            (parts.next()? == tarball_name).then(|| sum.to_string())
        })
        .ok_or_else(|| {
            format!("no checksum for {tarball_name} in vendor/libkrun-checksums.txt").into()
        })
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn locate_tarball(
    out_dir: &Path,
    version: &str,
    tarball_name: &str,
    expected: &str,
) -> BuildResult<PathBuf> {
    if let Some(path) = env::var_os(TARBALL_OVERRIDE_ENV).map(PathBuf::from) {
        let actual = sha256_file(&path)?;
        if actual != expected {
            return Err(format!(
                "{TARBALL_OVERRIDE_ENV}={} has sha256 {actual}, expected {expected}",
                path.display()
            )
            .into());
        }
        return Ok(path);
    }

    let cache_root = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_else(|| out_dir.to_path_buf())
        .join("ward-libkrun-cache");
    fs::create_dir_all(&cache_root)?;
    let cached = cache_root.join(tarball_name);
    if cached.exists() && sha256_file(&cached)? == expected {
        return Ok(cached);
    }

    let url = format!("{RELEASE_BASE}/libkrun-v{version}/{tarball_name}");
    let partial = cache_root.join(format!("{tarball_name}.partial"));
    {
        let response = ureq::get(&url)
            .call()
            .map_err(|e| format!("download {url}: {e}"))?;
        let mut body = response.into_body();
        let mut file = fs::File::create(&partial)?;
        io::copy(&mut body.as_reader(), &mut file)?;
    }
    let actual = sha256_file(&partial)?;
    if actual != expected {
        let _ = fs::remove_file(&partial);
        return Err(format!("{url}: sha256 {actual} does not match pinned {expected}").into());
    }
    fs::rename(&partial, &cached)?;
    Ok(cached)
}

/// libkrun loads its kernel with `dlopen("libkrunfw.5.dylib")`. The bottle
/// ships that dylib with install name `@rpath/libkrunfw.dylib`, which dyld
/// will not match against an image ward already loaded by absolute path.
/// Rewriting the install name lets ward preload the extracted copy so
/// libkrun's lookup resolves to it. The rewrite breaks the code signature,
/// so the dylib is ad-hoc signed again.
fn rename_macos_dylib(out_dir: &Path, original: &[u8]) -> BuildResult<Vec<u8>> {
    let mut bytes = original.to_vec();
    set_macho_install_name(&mut bytes, KRUNFW_MACOS_INSTALL_NAME)?;
    let scratch = out_dir.join("libkrunfw-resign.dylib");
    fs::write(&scratch, &bytes)?;
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-"])
        .arg(&scratch)
        .status()
        .map_err(|e| format!("run codesign (macOS builds need it to re-sign libkrunfw): {e}"))?;
    if !status.success() {
        return Err("codesign failed to re-sign libkrunfw".into());
    }
    Ok(fs::read(&scratch)?)
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn set_macho_install_name(bytes: &mut [u8], name: &str) -> BuildResult<()> {
    if read_u32(bytes, 0) != Some(MH_MAGIC_64) {
        return Err("libkrunfw is not a thin 64-bit little-endian Mach-O".into());
    }
    let ncmds = read_u32(bytes, MACHO_NCMDS_OFFSET).ok_or("truncated Mach-O header")?;
    let mut cursor = MACHO_HEADER_LEN;
    for _ in 0..ncmds {
        let cmd = read_u32(bytes, cursor).ok_or("truncated load command")?;
        let size = read_u32(bytes, cursor + 4).ok_or("truncated load command")? as usize;
        if cmd == LC_ID_DYLIB {
            let name_off = read_u32(bytes, cursor + DYLIB_NAME_FIELD_OFFSET)
                .ok_or("truncated dylib command")? as usize;
            let capacity = size.checked_sub(name_off).ok_or("bad dylib command")?;
            if name.len() + 1 > capacity {
                return Err("new install name does not fit in the load command".into());
            }
            let start = cursor + name_off;
            let field = bytes
                .get_mut(start..start + capacity)
                .ok_or("dylib name outside file")?;
            field.fill(0);
            field[..name.len()].copy_from_slice(name.as_bytes());
            return Ok(());
        }
        cursor += size;
    }
    Err("no LC_ID_DYLIB load command in libkrunfw".into())
}

fn build_agent(workspace: &Path, out_dir: &Path, guest_target: &str) -> BuildResult<Vec<u8>> {
    let target_dir = out_dir.join("agent-target");
    let run = || -> io::Result<std::process::Output> {
        let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cmd.current_dir(workspace)
            .args([
                "build",
                "--release",
                "-p",
                "ward-agent",
                "--target",
                guest_target,
            ])
            .arg("--target-dir")
            .arg(&target_dir)
            .env(
                format!(
                    "CARGO_TARGET_{}_LINKER",
                    guest_target.to_uppercase().replace('-', "_")
                ),
                "rust-lld",
            )
            .env("CARGO_PROFILE_RELEASE_STRIP", "symbols")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET");
        cmd.output()
    };

    let mut output = run()?;
    if !output.status.success() && missing_std(&output) {
        let added = Command::new("rustup")
            .args(["target", "add", guest_target])
            .status();
        if added.is_ok_and(|s| s.success()) {
            output = run()?;
        }
    }
    if !output.status.success() {
        return Err(format!(
            "building ward-agent for {guest_target} failed:\n{}\n\
             Install the target with `rustup target add {guest_target}`.",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(fs::read(
        target_dir
            .join(guest_target)
            .join("release")
            .join("ward-agent"),
    )?)
}

fn missing_std(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("can't find crate for `std`") || stderr.contains("can't find crate for `core`")
}
