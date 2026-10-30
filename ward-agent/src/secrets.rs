// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Secret files: written to a tmpfs under `/run/secrets` as read-only
//! files, so a secret never reaches the guest disk image.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, fchown};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::proto::SecretFile;
use crate::user::Identity;

const SECRETS_DIR: &str = "/run/secrets";
const DIR_MODE: u32 = 0o755;
const FILE_MODE: u32 = 0o400;
const MAX_NAME_BYTES: usize = 255;
/// Mounted closed; `install` opens it to 0755 so the recipient can traverse.
#[cfg(target_os = "linux")]
const MOUNT_OPTIONS: &std::ffi::CStr = c"mode=0700";
#[cfg(target_os = "linux")]
const TMPFS_MAGIC: rustix::fs::FsWord = 0x0102_1994;

/// Held while mounting, so two first deliveries cannot mount twice.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Write `files` into the guest's secrets directory, on a tmpfs.
pub(crate) fn install_in_guest(owner: Option<&Identity>, files: &[SecretFile]) -> io::Result<()> {
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = Path::new(SECRETS_DIR);
    fs::create_dir_all(dir)?;
    ensure_tmpfs(dir)?;
    install(dir, owner, files)
}

/// Write each file into `dir` as mode 0400, owned by `owner` or root. A
/// file that fails is removed, so no half-written secret stays behind.
pub(crate) fn install(
    dir: &Path,
    owner: Option<&Identity>,
    files: &[SecretFile],
) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    for file in files {
        check_name(&file.name)?;
        let path = dir.join(&file.name);
        let written = write_one(&path, owner, &file.content);
        if written.is_err() {
            let _ = fs::remove_file(&path);
        }
        written?;
    }
    Ok(())
}

fn write_one(path: &Path, owner: Option<&Identity>, content: &[u8]) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)?;
    if let Some(owner) = owner {
        fchown(&file, Some(owner.uid), Some(owner.gid))?;
    }
    file.write_all(content)?;
    file.sync_all()
}

fn check_name(name: &str) -> io::Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0']);
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid secret name {name:?}"),
        ))
    }
}

/// Mount a tmpfs on `dir` unless one is there already, since a second mount
/// would hide the secrets an earlier call wrote. Refuses to go on unless the
/// filesystem there is tmpfs, so a secret never reaches disk.
#[cfg(target_os = "linux")]
fn ensure_tmpfs(dir: &Path) -> io::Result<()> {
    use rustix::mount::{MountFlags, mount};

    if !is_tmpfs(dir)? {
        let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
        mount("tmpfs", dir, "tmpfs", flags, MOUNT_OPTIONS)
            .map_err(|e| io::Error::other(format!("mount tmpfs at {}: {e}", dir.display())))?;
    }
    if is_tmpfs(dir)? {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} is not a tmpfs, refusing to write secrets",
            dir.display()
        )))
    }
}

#[cfg(target_os = "linux")]
fn is_tmpfs(dir: &Path) -> io::Result<bool> {
    let stat = rustix::fs::statfs(dir).map_err(io::Error::from)?;
    Ok(stat.f_type == TMPFS_MAGIC)
}

#[cfg(not(target_os = "linux"))]
fn ensure_tmpfs(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::os::unix::fs::MetadataExt;

    fn secret(name: &str, content: &[u8]) -> SecretFile {
        SecretFile {
            name: name.into(),
            content: content.to_vec(),
        }
    }

    #[test]
    fn given_a_secret_when_installed_then_the_file_is_read_only_with_its_content() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let files = [secret("token", b"s3cret")];

        // Act
        install(dir.path(), None, &files).expect("install");

        // Assert
        let path = dir.path().join("token");
        assert_eq!(fs::read(&path).unwrap(), b"s3cret");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, FILE_MODE);
    }

    #[test]
    fn given_an_existing_secret_when_installed_again_then_it_is_replaced() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        install(dir.path(), None, &[secret("token", b"old")]).expect("first");

        // Act
        install(dir.path(), None, &[secret("token", b"new")]).expect("second");

        // Assert
        assert_eq!(fs::read(dir.path().join("token")).unwrap(), b"new");
    }

    #[rstest]
    #[case::empty("")]
    #[case::dot(".")]
    #[case::dot_dot("..")]
    #[case::slash("a/b")]
    #[case::nul("a\0b")]
    fn given_an_unsafe_name_when_installed_then_it_is_refused(#[case] name: &str) {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");

        // Act
        let result = install(dir.path(), None, &[secret(name, b"x")]);

        // Assert
        assert!(result.is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
