// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Tmpfs mounts and the read-only root filesystem, set up before the agent
//! serves its first process.

/// Argument that mounts the root filesystem read-only.
pub const READ_ONLY_ARG: &str = "--read-only";
/// Argument that mounts a tmpfs: `--tmpfs <path> <options>`.
pub const TMPFS_ARG: &str = "--tmpfs";
/// Argument that sizes `/dev/shm`: `--shm-size <bytes>`.
pub const SHM_SIZE_ARG: &str = "--shm-size";

/// Where Docker mounts the shared memory tmpfs.
#[cfg(target_os = "linux")]
const SHM_PATH: &str = "/dev/shm";
/// Mode of `/dev/shm`, as Docker sets it.
#[cfg(target_os = "linux")]
const SHM_MODE: &str = "1777";
/// Directories that stay writable when the root filesystem is read-only.
#[cfg(any(target_os = "linux", test))]
const WRITABLE_DIRS: [&str; 2] = ["/tmp", "/run"];
/// Mode of those directories.
#[cfg(target_os = "linux")]
const WRITABLE_DIR_MODE: &str = "1777";

/// A tmpfs the daemon asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestTmpfs {
    pub target: String,
    /// Mount options as Docker's `--tmpfs` takes them.
    pub options: String,
}

/// How a tmpfs is mounted: the flags, and the data string for the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsPlan {
    pub read_only: bool,
    pub nosuid: bool,
    pub nodev: bool,
    pub noexec: bool,
    pub sync: bool,
    pub dirsync: bool,
    pub noatime: bool,
    pub nodiratime: bool,
    pub strictatime: bool,
    pub data: String,
}

/// Docker mounts a tmpfs `noexec,nosuid,nodev` unless an option says otherwise.
impl Default for TmpfsPlan {
    fn default() -> Self {
        Self {
            read_only: false,
            nosuid: true,
            nodev: true,
            noexec: true,
            sync: false,
            dirsync: false,
            noatime: false,
            nodiratime: false,
            strictatime: false,
            data: String::new(),
        }
    }
}

impl TmpfsPlan {
    /// Read Docker `--tmpfs` options on top of the defaults. Later options win.
    ///
    /// # Errors
    ///
    /// Fails on an option the agent does not know.
    pub fn parse(options: &str) -> Result<Self, String> {
        let mut plan = Self::default();
        let mut data = Vec::new();
        for option in options.split(',').filter(|o| !o.is_empty()) {
            match option {
                "ro" => plan.read_only = true,
                "rw" => plan.read_only = false,
                "suid" => plan.nosuid = false,
                "nosuid" => plan.nosuid = true,
                "dev" => plan.nodev = false,
                "nodev" => plan.nodev = true,
                "exec" => plan.noexec = false,
                "noexec" => plan.noexec = true,
                "sync" => plan.sync = true,
                "async" => plan.sync = false,
                "dirsync" => plan.dirsync = true,
                "atime" => plan.noatime = false,
                "noatime" => plan.noatime = true,
                "diratime" => plan.nodiratime = false,
                "nodiratime" => plan.nodiratime = true,
                "strictatime" => plan.strictatime = true,
                "relatime" | "norelatime" | "nostrictatime" => {}
                other => match other.split_once('=') {
                    Some((key, _)) if DATA_KEYS.contains(&key) => data.push(other),
                    _ => return Err(format!("unknown tmpfs option '{other}'")),
                },
            }
        }
        plan.data = data.join(",");
        Ok(plan)
    }
}

/// Filesystem data options the kernel's tmpfs reads.
const DATA_KEYS: [&str; 5] = ["size", "mode", "nr_inodes", "uid", "gid"];

/// The tmpfs mounts a read-only root filesystem keeps writable, minus the
/// paths something else already covers.
#[cfg(any(target_os = "linux", test))]
fn implicit_writable_dirs(taken: &[&str]) -> Vec<&'static str> {
    WRITABLE_DIRS
        .into_iter()
        .filter(|dir| !taken.contains(dir))
        .collect()
}

#[cfg(target_os = "linux")]
fn flags_of(plan: &TmpfsPlan) -> rustix::mount::MountFlags {
    use rustix::mount::MountFlags;
    [
        (plan.read_only, MountFlags::RDONLY),
        (plan.nosuid, MountFlags::NOSUID),
        (plan.nodev, MountFlags::NODEV),
        (plan.noexec, MountFlags::NOEXEC),
        (plan.sync, MountFlags::SYNCHRONOUS),
        (plan.dirsync, MountFlags::DIRSYNC),
        (plan.noatime, MountFlags::NOATIME),
        (plan.nodiratime, MountFlags::NODIRATIME),
        (plan.strictatime, MountFlags::STRICTATIME),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .fold(MountFlags::empty(), |all, (_, flag)| all | flag)
}

#[cfg(target_os = "linux")]
fn mount_tmpfs(target: &str, plan: &TmpfsPlan) -> std::io::Result<()> {
    let data = std::ffi::CString::new(plan.data.as_str())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    std::fs::create_dir_all(target)?;
    rustix::mount::mount("tmpfs", target, "tmpfs", flags_of(plan), data.as_c_str())
        .map_err(|e| std::io::Error::other(format!("mount tmpfs at {target}: {e}")))
}

/// Mount the requested tmpfs filesystems, size `/dev/shm`, and, for a
/// read-only root, give `/tmp` and `/run` a tmpfs unless a mount is there.
///
/// # Errors
///
/// Fails on the first mount that the kernel or the options refuse.
#[cfg(target_os = "linux")]
pub fn mount_tmpfs_all(
    tmpfs: &[GuestTmpfs],
    shm_size: Option<u64>,
    read_only: bool,
    bind_targets: &[&str],
) -> std::io::Result<()> {
    for mount in tmpfs {
        let plan = TmpfsPlan::parse(&mount.options)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        mount_tmpfs(&mount.target, &plan)?;
    }
    if let Some(size) = shm_size {
        let plan = TmpfsPlan {
            data: format!("size={size},mode={SHM_MODE}"),
            ..TmpfsPlan::default()
        };
        mount_tmpfs(SHM_PATH, &plan)?;
    }
    if read_only {
        let taken: Vec<&str> = tmpfs
            .iter()
            .map(|m| m.target.as_str())
            .chain(bind_targets.iter().copied())
            .collect();
        for dir in implicit_writable_dirs(&taken) {
            let plan = TmpfsPlan {
                noexec: false,
                data: format!("mode={WRITABLE_DIR_MODE}"),
                ..TmpfsPlan::default()
            };
            mount_tmpfs(dir, &plan)?;
        }
    }
    Ok(())
}

/// Make the root filesystem read-only, keeping the mount flags it has.
///
/// # Errors
///
/// Fails when the kernel refuses, for example because a file is open for
/// writing.
#[cfg(target_os = "linux")]
pub fn remount_root_read_only() -> std::io::Result<()> {
    use rustix::fs::StatVfsMountFlags;
    use rustix::mount::{MountFlags, mount_remount};

    const ROOT: &str = "/";
    let current = rustix::fs::statvfs(ROOT)?.f_flag;
    let kept = [
        (StatVfsMountFlags::NOSUID, MountFlags::NOSUID),
        (StatVfsMountFlags::NODEV, MountFlags::NODEV),
        (StatVfsMountFlags::NOEXEC, MountFlags::NOEXEC),
        (StatVfsMountFlags::NOATIME, MountFlags::NOATIME),
        (StatVfsMountFlags::NODIRATIME, MountFlags::NODIRATIME),
        (StatVfsMountFlags::RELATIME, MountFlags::RELATIME),
    ]
    .into_iter()
    .filter(|(have, _)| current.contains(*have))
    .fold(MountFlags::RDONLY, |all, (_, flag)| all | flag);
    mount_remount(ROOT, kept, "")
        .map_err(|e| std::io::Error::other(format!("remount / read-only: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_no_options_when_parsed_then_tmpfs_is_noexec_nosuid_nodev_like_docker() {
        // Act
        let plan = TmpfsPlan::parse("").unwrap();

        // Assert
        assert_eq!(plan, TmpfsPlan::default());
        assert!(plan.noexec && plan.nosuid && plan.nodev && !plan.read_only);
    }

    #[test]
    fn given_options_when_parsed_then_later_ones_win_and_data_is_kept_in_order() {
        // Act
        let plan = TmpfsPlan::parse("ro,rw,exec,suid,size=64m,mode=0755,noatime").unwrap();

        // Assert
        assert!(!plan.read_only && !plan.noexec && !plan.nosuid && plan.noatime);
        assert_eq!(plan.data, "size=64m,mode=0755");
    }

    #[rstest::rstest]
    #[case::unknown_flag("bogus")]
    #[case::unknown_key("color=red")]
    fn given_an_unknown_option_when_parsed_then_it_is_refused(#[case] options: &str) {
        assert!(TmpfsPlan::parse(options).is_err());
    }

    #[test]
    fn given_a_mount_on_tmp_when_listing_implicit_dirs_then_only_run_is_left() {
        // Act
        let dirs = implicit_writable_dirs(&["/tmp"]);

        // Assert
        assert_eq!(dirs, ["/run"]);
        assert_eq!(implicit_writable_dirs(&[]), ["/tmp", "/run"]);
    }
}
