// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Metadata a commit keeps beyond file contents: extended attributes,
//! modification times, sparse extents and special files.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};

use rustix::fs::{self, AtFlags, FileType, Mode, SeekFrom, Stat, Timespec, Timestamps};
use rustix::io::Errno;

/// Extended attribute namespace a guest process can set without privilege.
const USER_XATTR_PREFIX: &[u8] = b"user.";
/// File capabilities, kept like Docker keeps them in a layer.
const CAPABILITY_XATTR: &[u8] = b"security.capability";

/// Whether a commit carries the extended attribute `name`. Other security
/// and system namespaces describe the guest kernel's state, not the image.
fn is_preserved_xattr(name: &[u8]) -> bool {
    name.starts_with(USER_XATTR_PREFIX) || name == CAPABILITY_XATTR
}

/// Copy the preserved extended attributes of `from` onto `to`. Best effort:
/// a filesystem without them, or a daemon that may not set file
/// capabilities, loses them without failing the commit.
pub(super) fn copy_xattrs(from: impl AsFd, to: impl AsFd) {
    let Ok(size) = fs::flistxattr(&from, &mut [] as &mut [u8]) else {
        return;
    };
    let mut names = vec![0u8; size];
    let Ok(len) = fs::flistxattr(&from, names.as_mut_slice()) else {
        return;
    };
    for name in names[..len].split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if !is_preserved_xattr(name) {
            continue;
        }
        let Ok(size) = fs::fgetxattr(&from, name, &mut [] as &mut [u8]) else {
            continue;
        };
        let mut value = vec![0u8; size];
        let Ok(len) = fs::fgetxattr(&from, name, value.as_mut_slice()) else {
            continue;
        };
        let _ = fs::fsetxattr(&to, name, &value[..len], fs::XattrFlags::empty());
    }
}

/// Access and modification time both set to the whole-second mtime of
/// `stat`, so equal sources give equal copies.
fn whole_second_times(stat: &Stat) -> Timestamps {
    let time = Timespec {
        tv_sec: stat.st_mtime,
        tv_nsec: 0,
    };
    Timestamps {
        last_access: time,
        last_modification: time,
    }
}

pub(super) fn restore_times(fd: impl AsFd, stat: &Stat) -> Result<(), Errno> {
    fs::futimens(fd, &whole_second_times(stat))
}

/// Set the times of the link or node `name` itself, never of a target.
pub(super) fn restore_times_at(dir: &OwnedFd, name: &OsStr, stat: &Stat) -> Result<(), Errno> {
    fs::utimensat(
        dir,
        name,
        &whole_second_times(stat),
        AtFlags::SYMLINK_NOFOLLOW,
    )
}

/// Next run of file data at or after `pos`, as `(start, end)` bounded by
/// `size`. A filesystem that cannot report holes yields one run.
pub(super) fn next_data_run(file: &File, pos: u64, size: u64) -> Result<Option<(u64, u64)>, Errno> {
    if pos >= size {
        return Ok(None);
    }
    let start = match fs::seek(file, SeekFrom::Data(pos)) {
        Ok(start) => start,
        Err(Errno::NXIO) => return Ok(None),
        Err(Errno::INVAL | Errno::OPNOTSUPP | Errno::NOSYS) => return Ok(Some((pos, size))),
        Err(errno) => return Err(errno),
    };
    if start >= size {
        return Ok(None);
    }
    let end = fs::seek(file, SeekFrom::Hole(start)).map_or(size, |end| end.min(size));
    Ok(Some((start, end)))
}

/// Recreate the FIFO or device node `name` of kind `kind`. Returns `false`
/// when the node is left out: the daemon may not create it, which is how a
/// device node fails without privilege, or the host has no `mknodat`.
#[cfg(not(target_vendor = "apple"))]
pub(super) fn make_special(
    dst: &OwnedFd,
    name: &OsStr,
    kind: FileType,
    mode: Mode,
    stat: &Stat,
) -> Result<bool, Errno> {
    // `st_rdev` is a narrower signed type on some hosts.
    #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
    let device = stat.st_rdev as fs::Dev;
    match fs::mknodat(dst, name, kind, mode, device) {
        Ok(()) => {}
        Err(Errno::PERM | Errno::ACCESS) => return Ok(false),
        Err(errno) => return Err(errno),
    }
    // The creation mask trimmed the mode, so set the recorded one.
    fs::chmodat(dst, name, mode, AtFlags::empty())?;
    Ok(true)
}

/// macOS has no `mknodat`, so FIFOs and device nodes are left out there.
#[cfg(target_vendor = "apple")]
pub(super) fn make_special(
    _dst: &OwnedFd,
    _name: &OsStr,
    _kind: FileType,
    _mode: Mode,
    _stat: &Stat,
) -> Result<bool, Errno> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::fs::OpenOptions;

    #[rstest]
    #[case::user(b"user.note", true)]
    #[case::capability(b"security.capability", true)]
    #[case::selinux(b"security.selinux", false)]
    #[case::trusted(b"trusted.overlay.opaque", false)]
    #[case::acl(b"system.posix_acl_access", false)]
    fn given_an_xattr_name_when_filtered_then_only_portable_ones_are_kept(
        #[case] name: &[u8],
        #[case] kept: bool,
    ) {
        // Act
        let result = is_preserved_xattr(name);

        // Assert
        assert_eq!(result, kept);
    }

    #[test]
    fn given_a_user_xattr_when_copied_then_the_copy_has_it() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let open = |name: &str| {
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(dir.path().join(name))
                .expect("open")
        };
        let (from, to) = (open("from"), open("to"));
        if fs::fsetxattr(&from, "user.ward", b"v1", fs::XattrFlags::empty()).is_err() {
            return; // this filesystem has no user xattrs
        }

        // Act
        copy_xattrs(&from, &to);

        // Assert
        let mut value = [0u8; 8];
        let len = fs::fgetxattr(&to, "user.ward", value.as_mut_slice()).expect("xattr");
        assert_eq!(&value[..len], b"v1");
    }

    #[test]
    fn given_a_mtime_with_nanoseconds_when_converted_then_they_are_dropped() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let file = File::create(dir.path().join("f")).expect("file");
        let time = Timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 123_456_789,
        };
        fs::futimens(
            &file,
            &Timestamps {
                last_access: time,
                last_modification: time,
            },
        )
        .expect("set times");
        let stat = fs::fstat(&file).expect("stat");

        // Act
        let times = whole_second_times(&stat);

        // Assert
        assert_eq!(times.last_modification.tv_sec, 1_700_000_000);
        assert_eq!(times.last_modification.tv_nsec, 0);
        assert_eq!(times.last_access.tv_nsec, 0);
    }
}
