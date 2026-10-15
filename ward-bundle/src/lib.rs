// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Self-contained runtime payloads for ward.
//!
//! With the `embed` feature this crate carries libkrun, libkrunfw and the
//! guest agent inside the binary. At runtime they are unpacked once into a
//! per-user directory, so nothing has to be installed on the host.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

include!(concat!(env!("OUT_DIR"), "/bundle_consts.rs"));

static KRUN_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libkrun.gz"));
static KRUNFW_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libkrunfw.gz"));
static AGENT_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ward-agent.gz"));

/// Environment variable that overrides where the libraries are unpacked.
pub const RUNTIME_DIR_ENV: &str = "WARD_RUNTIME_DIR";

const LIB_FILE_MODE: u32 = 0o755;
const RUNTIME_DIR_MODE: u32 = 0o700;
const HOME_RUNTIME_SUBDIR: &str = ".ward/runtime";

/// Absolute paths of the unpacked shared libraries.
#[derive(Debug, Clone)]
pub struct LibPaths {
    pub krun: PathBuf,
    pub krunfw: PathBuf,
}

/// True when libkrun and libkrunfw are embedded for this build target.
#[must_use]
pub fn libs_available() -> bool {
    SUPPORTED
}

/// Directory ward unpacks the embedded libraries into by default.
#[must_use]
pub fn default_runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(RUNTIME_DIR_ENV) {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(HOME_RUNTIME_SUBDIR),
        None => std::env::temp_dir().join("ward-runtime"),
    }
}

/// Unpack libkrun and libkrunfw under `base` (idempotent) and return their paths.
///
/// # Errors
///
/// Fails when the build carries no libraries for this target or when the
/// directory cannot be written.
pub fn extract_libs(base: &Path) -> io::Result<LibPaths> {
    if !SUPPORTED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this ward build does not embed libkrun (unsupported target or built without the `embed` feature)",
        ));
    }
    let final_dir = base.join(format!("libs-{BUNDLE_ID}"));
    let paths = LibPaths {
        krun: final_dir.join(KRUN_FILE_NAME),
        krunfw: final_dir.join(KRUNFW_FILE_NAME),
    };
    if paths.krun.is_file() && paths.krunfw.is_file() {
        return Ok(paths);
    }

    fs::create_dir_all(base)?;
    set_mode(base, RUNTIME_DIR_MODE)?;
    let staging = base.join(format!(".libs-{BUNDLE_ID}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    write_payload(&staging.join(KRUN_FILE_NAME), KRUN_GZ)?;
    write_payload(&staging.join(KRUNFW_FILE_NAME), KRUNFW_GZ)?;
    match fs::rename(&staging, &final_dir) {
        Ok(()) => Ok(paths),
        Err(_) if paths.krun.is_file() && paths.krunfw.is_file() => {
            // Another process finished first.
            let _ = fs::remove_dir_all(&staging);
            Ok(paths)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// The static Linux guest agent binary for this host's architecture.
///
/// # Errors
///
/// Fails when the build carries no agent.
pub fn guest_agent() -> io::Result<Vec<u8>> {
    if !HAS_AGENT {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this ward build does not embed the guest agent",
        ));
    }
    let mut out = Vec::new();
    GzDecoder::new(AGENT_GZ).read_to_end(&mut out)?;
    Ok(out)
}

fn write_payload(path: &Path, gz: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    let mut decoder = GzDecoder::new(gz);
    io::copy(&mut decoder, &mut file)?;
    file.flush()?;
    set_mode(path, LIB_FILE_MODE)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_runtime_dir_env_when_default_runtime_dir_then_env_wins() {
        // Arrange / Act / Assert: serial-safe because it only reads the env
        // value it set itself and restores it.
        let key = RUNTIME_DIR_ENV;
        let previous = std::env::var_os(key);
        // SAFETY: tests in this module do not run other env-reading code
        // concurrently with this one.
        unsafe { std::env::set_var(key, "/custom/ward-runtime") };
        let dir = default_runtime_dir();
        match previous {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        assert_eq!(dir, PathBuf::from("/custom/ward-runtime"));
    }

    #[test]
    fn given_build_without_embedded_libs_when_extract_then_unsupported_error() {
        if libs_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_libs(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn given_embedded_libs_when_extract_twice_then_same_paths_and_real_libraries() {
        if !libs_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        let first = extract_libs(tmp.path()).unwrap();
        let second = extract_libs(tmp.path()).unwrap();

        assert_eq!(first.krun, second.krun);
        assert_eq!(first.krunfw, second.krunfw);
        for lib in [&first.krun, &first.krunfw] {
            let bytes = fs::read(lib).unwrap();
            assert!(bytes.len() > 1_000_000, "{lib:?} is suspiciously small");
            let is_elf = bytes.starts_with(b"\x7fELF");
            let is_macho = bytes.starts_with(&0xfeed_facf_u32.to_le_bytes());
            assert!(is_elf || is_macho, "{lib:?} is not an ELF or Mach-O file");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn given_embedded_libkrunfw_on_macos_when_extracted_then_install_name_is_the_leaf_dyld_searches_for()
     {
        if !libs_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let libs = extract_libs(tmp.path()).unwrap();

        let out = std::process::Command::new("otool")
            .arg("-D")
            .arg(&libs.krunfw)
            .output()
            .unwrap();

        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.lines()
                .last()
                .is_some_and(|l| l.trim() == KRUNFW_FILE_NAME),
            "unexpected install name: {text}"
        );
    }

    #[test]
    fn given_embedded_agent_when_read_then_it_is_a_static_linux_elf() {
        if !HAS_AGENT {
            return;
        }
        let agent = guest_agent().unwrap();
        assert!(agent.starts_with(b"\x7fELF"));
    }
}
