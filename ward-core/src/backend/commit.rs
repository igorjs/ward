// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Copy of a sandbox rootfs into a new image rootfs.
//!
//! The guest controls the source tree and may change it during the copy, so
//! every step goes through directory fds with `O_NOFOLLOW`: a symlink is
//! copied as a link and never followed, and an entry swapped for another
//! type after it was listed is skipped. Owner and mode come from the opened
//! fd. Setuid and setgid bits are dropped, as the snapshot policy does.
//! Device nodes, FIFOs and sockets are not copied. Hard links become
//! separate files, and extended attributes, sparse holes and modification
//! times are not kept. An entry the daemon cannot read fails the commit.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{self, AtFlags, FileType, Gid, Mode, OFlags, Stat, Uid};
use rustix::io::Errno;

use super::diff::{DIR_FLAGS, file_type, list_names, open_file, open_root, stat_at};
use super::image::ImageConfig;
use super::{BackendError, Result};
use crate::protocol::SandboxSpec;

/// Most entries one commit visits.
pub const MAX_COMMIT_ENTRIES: usize = 500_000;
/// Deepest directory nesting one commit descends.
pub const MAX_COMMIT_DEPTH: usize = 64;
/// Most file bytes one commit copies.
pub const MAX_COMMIT_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Bytes moved per read while copying a file.
const COPY_CHUNK_BYTES: usize = 64 * 1024;
/// Flags for creating the copy of a file: never replace or follow anything.
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// Mode bits that make a program run as its owner or group.
const PRIVILEGE_BITS: Mode = Mode::SUID.union(Mode::SGID);

/// Caps on one copy.
#[derive(Debug, Clone, Copy)]
pub struct CopyLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_bytes: u64,
}

impl Default for CopyLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_COMMIT_ENTRIES,
            max_depth: MAX_COMMIT_DEPTH,
            max_bytes: MAX_COMMIT_BYTES,
        }
    }
}

/// Why a copy stopped.
#[derive(Debug)]
pub enum CopyError {
    /// The file contents passed `CopyLimits::max_bytes`.
    TooLarge,
    /// The caller set the cancel flag.
    Cancelled,
    Backend(BackendError),
}

/// An I/O failure on the guest path `rel`, named in the message.
fn unreadable(rel: &Path, error: impl Into<std::io::Error>) -> CopyError {
    let error = error.into();
    CopyError::Backend(BackendError::Io(std::io::Error::new(
        error.kind(),
        format!("cannot copy /{}: {error}", rel.display()),
    )))
}

/// Copy the tree at `src` into the existing, empty directory `dst` and return
/// the bytes of file content copied. `skip` holds guest paths the backend
/// writes itself, left out of the copy. The copy stops once `cancel` is set.
pub fn copy_tree(
    src: &Path,
    dst: &Path,
    skip: &[&str],
    limits: CopyLimits,
    cancel: &AtomicBool,
) -> std::result::Result<u64, CopyError> {
    let mut copy = Copy {
        skip,
        limits,
        cancel,
        visited: 0,
        copied_bytes: 0,
    };
    let (src, dst) = (
        open_root(src).map_err(CopyError::Backend)?,
        open_root(dst).map_err(CopyError::Backend)?,
    );
    copy.copy_dir(&src, &dst, Path::new(""), 1)?;
    Ok(copy.copied_bytes)
}

struct Copy<'a> {
    skip: &'a [&'a str],
    limits: CopyLimits,
    cancel: &'a AtomicBool,
    visited: usize,
    copied_bytes: u64,
}

impl Copy<'_> {
    fn copy_dir(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        rel: &Path,
        depth: usize,
    ) -> std::result::Result<(), CopyError> {
        for name in list_names(src).map_err(CopyError::Backend)? {
            let child = rel.join(&name);
            if self.is_skipped(&child) {
                continue;
            }
            self.visit(depth)?;
            let Some(stat) = stat_at(src, &name).map_err(|e| unreadable(&child, e))? else {
                continue;
            };
            match file_type(&stat) {
                FileType::Directory => self.copy_subdir(src, dst, &name, &child, depth)?,
                FileType::RegularFile => self.copy_file(src, dst, &name, &child)?,
                FileType::Symlink => copy_link(src, dst, &name, &child, &stat)?,
                // FIFOs, sockets and devices are recreated by the guest.
                _ => {}
            }
        }
        Ok(())
    }

    /// A directory that vanished or was swapped for a link since it was
    /// listed is left out.
    fn copy_subdir(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        name: &OsStr,
        rel: &Path,
        depth: usize,
    ) -> std::result::Result<(), CopyError> {
        let inner = match fs::openat(src, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => return Ok(()),
            Err(errno) => return Err(unreadable(rel, errno)),
        };
        let stat = fs::fstat(&inner).map_err(|e| unreadable(rel, e))?;
        // The owner can always enter and fill the copy; the real mode is set
        // once its children are in.
        fs::mkdirat(dst, name, Mode::RWXU).map_err(|e| unreadable(rel, e))?;
        let copy =
            fs::openat(dst, name, DIR_FLAGS, Mode::empty()).map_err(|e| unreadable(rel, e))?;
        self.copy_dir(&inner, &copy, rel, depth + 1)?;
        restore_owner_and_mode(&copy, &stat).map_err(|e| unreadable(rel, e))
    }

    /// A file that vanished or was swapped for a link or special file since
    /// it was listed is left out.
    fn copy_file(
        &mut self,
        src: &OwnedFd,
        dst: &OwnedFd,
        name: &OsStr,
        rel: &Path,
    ) -> std::result::Result<(), CopyError> {
        let mut input = match open_file(src, name) {
            Ok(file) => file,
            Err(Errno::NOENT | Errno::LOOP) => return Ok(()),
            Err(errno) => return Err(unreadable(rel, errno)),
        };
        // A pipe or device swapped in after the listing is never read.
        let stat = fs::fstat(&input).map_err(|e| unreadable(rel, e))?;
        if file_type(&stat) != FileType::RegularFile {
            return Ok(());
        }
        let mut output = File::from(
            fs::openat(dst, name, CREATE_FLAGS, Mode::RUSR | Mode::WUSR)
                .map_err(|e| unreadable(rel, e))?,
        );
        let mut chunk = vec![0u8; COPY_CHUNK_BYTES];
        loop {
            self.check_cancelled()?;
            let read = input.read(&mut chunk).map_err(|e| unreadable(rel, e))?;
            if read == 0 {
                break;
            }
            self.copied_bytes += read as u64;
            if self.copied_bytes > self.limits.max_bytes {
                return Err(CopyError::TooLarge);
            }
            output
                .write_all(&chunk[..read])
                .map_err(|e| unreadable(rel, e))?;
        }
        restore_owner_and_mode(&output, &stat).map_err(|e| unreadable(rel, e))
    }

    fn is_skipped(&self, rel: &Path) -> bool {
        let guest_path = Path::new("/").join(rel);
        self.skip
            .iter()
            .any(|skipped| Path::new(skipped) == guest_path)
    }

    fn check_cancelled(&self) -> std::result::Result<(), CopyError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        Ok(())
    }

    fn visit(&mut self, depth: usize) -> std::result::Result<(), CopyError> {
        self.check_cancelled()?;
        if depth > self.limits.max_depth {
            return Err(CopyError::Backend(BackendError::ResourceExhausted(
                format!(
                    "the filesystem nests deeper than {} directories",
                    self.limits.max_depth
                ),
            )));
        }
        self.visited += 1;
        if self.visited > self.limits.max_entries {
            return Err(CopyError::Backend(BackendError::ResourceExhausted(
                format!(
                    "the filesystem has more than {} entries to commit",
                    self.limits.max_entries
                ),
            )));
        }
        Ok(())
    }
}

/// Recreate the link `name` of `src` in `dst`. The target text is copied and
/// never resolved.
fn copy_link(
    src: &OwnedFd,
    dst: &OwnedFd,
    name: &OsStr,
    rel: &Path,
    stat: &Stat,
) -> std::result::Result<(), CopyError> {
    let target = match fs::readlinkat(src, name, Vec::new()) {
        Ok(target) => target,
        Err(Errno::NOENT | Errno::INVAL) => return Ok(()),
        Err(errno) => return Err(unreadable(rel, errno)),
    };
    fs::symlinkat(target.as_c_str(), dst, name).map_err(|e| unreadable(rel, e))?;
    // Best effort: only a privileged daemon may give the link away.
    let _ = fs::chownat(
        dst,
        name,
        Some(Uid::from_raw(stat.st_uid)),
        Some(Gid::from_raw(stat.st_gid)),
        AtFlags::SYMLINK_NOFOLLOW,
    );
    Ok(())
}

/// Give the copy the owner and mode of the inode that was copied, without
/// setuid and setgid. Ownership only changes for a privileged daemon.
fn restore_owner_and_mode(fd: impl AsFd, stat: &Stat) -> std::result::Result<(), Errno> {
    let _ = fs::fchown(
        &fd,
        Some(Uid::from_raw(stat.st_uid)),
        Some(Gid::from_raw(stat.st_gid)),
    );
    fs::fchmod(
        &fd,
        Mode::from_raw_mode(stat.st_mode).difference(PRIVILEGE_BITS),
    )
}

/// What a commit records besides the files.
#[derive(Debug, Clone, Default)]
pub struct CommitSpec {
    /// Local tag for the image. `None` leaves it addressable by digest only.
    pub reference: Option<String>,
    pub author: String,
    pub message: String,
    /// Dockerfile instructions applied to the image config.
    pub changes: Vec<String>,
}

/// Shell that runs a `CMD` or `ENTRYPOINT` written without a JSON array.
const SHELL_FORM: [&str; 2] = ["/bin/sh", "-c"];

/// Apply Dockerfile `changes` to `config`, in order. Only CMD, ENTRYPOINT,
/// ENV, WORKDIR and USER are understood; anything else is refused.
pub fn apply_changes(config: &mut ImageConfig, changes: &[String]) -> Result<()> {
    let mut cmd_set = false;
    for change in changes {
        let unsupported = || BackendError::InvalidArgument(format!("unsupported change: {change}"));
        let (keyword, rest) = change
            .trim()
            .split_once(char::is_whitespace)
            .ok_or_else(unsupported)?;
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(unsupported());
        }
        match keyword.to_ascii_uppercase().as_str() {
            "CMD" => {
                config.cmd = command_words(rest)?;
                cmd_set = true;
            }
            "ENTRYPOINT" => {
                config.entrypoint = command_words(rest)?;
                if !cmd_set {
                    config.cmd.clear();
                }
            }
            "ENV" => set_env(&mut config.env, rest).ok_or_else(unsupported)?,
            "WORKDIR" => {
                let base = Path::new("/").join(&config.working_dir);
                config.working_dir = base.join(rest).to_string_lossy().into_owned();
            }
            "USER" => config.user = rest.to_string(),
            _ => return Err(unsupported()),
        }
    }
    Ok(())
}

/// A JSON array as is, anything else as one shell command line.
fn command_words(text: &str) -> Result<Vec<String>> {
    if text.starts_with('[') {
        return serde_json::from_str(text).map_err(|e| {
            BackendError::InvalidArgument(format!("command is not a JSON array of strings: {e}"))
        });
    }
    Ok(SHELL_FORM
        .iter()
        .map(|word| (*word).to_string())
        .chain([text.to_string()])
        .collect())
}

/// `ENV KEY=VALUE [KEY=VALUE ...]` with shell quoting, or the legacy
/// `ENV KEY value`. `None` if malformed.
fn set_env(env: &mut Vec<String>, text: &str) -> Option<()> {
    let words = shlex::split(text)?;
    let pairs: Vec<(&str, &str)> = if words.first()?.contains('=') {
        words
            .iter()
            .map(|word| word.split_once('='))
            .collect::<Option<_>>()?
    } else {
        vec![text.split_once(char::is_whitespace)?]
    };
    for (key, value) in pairs {
        if key.is_empty() {
            return None;
        }
        put_env(env, key, value.trim());
    }
    Some(())
}

fn put_env(env: &mut Vec<String>, key: &str, value: &str) {
    let prefix = format!("{key}=");
    env.retain(|entry| !entry.starts_with(&prefix));
    env.push(format!("{prefix}{value}"));
}

/// Lay the sandbox's own start settings over the image config it booted
/// from. A sandbox without a main process keeps the image's command.
pub fn overlay_sandbox(
    config: &mut ImageConfig,
    spec: &SandboxSpec,
    env: &std::collections::HashMap<String, String>,
) {
    if !spec.command.is_empty() {
        let (entrypoint, cmd) = spec
            .command
            .split_at(spec.entrypoint_len.min(spec.command.len()));
        config.entrypoint = entrypoint.to_vec();
        config.cmd = cmd.to_vec();
    }
    if !spec.workdir.is_empty() {
        config.working_dir.clone_from(&spec.workdir);
    }
    if !spec.user.is_empty() {
        config.user.clone_from(&spec.user);
    }
    let mut vars: Vec<_> = env.iter().collect();
    vars.sort();
    for (key, value) in vars {
        put_env(&mut config.env, key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn tree() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let (src, dst) = (root.path().join("src"), root.path().join("dst"));
        std::fs::create_dir_all(&src).expect("src");
        std::fs::create_dir_all(&dst).expect("dst");
        (root, src, dst)
    }

    fn copy(
        src: &Path,
        dst: &Path,
        skip: &[&str],
        limits: CopyLimits,
    ) -> std::result::Result<u64, CopyError> {
        copy_tree(src, dst, skip, limits, &AtomicBool::new(false))
    }

    #[test]
    fn given_files_and_dirs_when_copied_then_bytes_and_modes_match() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::create_dir_all(src.join("a/b")).expect("dirs");
        std::fs::write(src.join("a/b/f"), b"known bytes").expect("file");
        std::fs::set_permissions(src.join("a/b/f"), std::fs::Permissions::from_mode(0o751))
            .expect("mode");

        // Act
        copy(&src, &dst, &[], CopyLimits::default()).expect("copy");

        // Assert
        assert_eq!(
            std::fs::read(dst.join("a/b/f")).expect("read"),
            b"known bytes"
        );
        let mode = std::fs::metadata(dst.join("a/b/f"))
            .expect("meta")
            .permissions();
        assert_eq!(mode.mode() & 0o7777, 0o751);
    }

    #[test]
    fn given_a_symlink_to_a_host_file_when_copied_then_a_link_is_kept_not_the_bytes() {
        // Arrange
        let (root, src, dst) = tree();
        let host = root.path().join("host-secret");
        std::fs::write(&host, b"host bytes").expect("host file");
        symlink(&host, src.join("link")).expect("link");
        symlink("../escape", src.join("dangling")).expect("link");

        // Act
        copy(&src, &dst, &[], CopyLimits::default()).expect("copy");

        // Assert
        let meta = std::fs::symlink_metadata(dst.join("link")).expect("meta");
        assert!(meta.file_type().is_symlink());
        assert_eq!(std::fs::read_link(dst.join("link")).expect("target"), host);
        assert_eq!(
            std::fs::read_link(dst.join("dangling")).expect("target"),
            Path::new("../escape")
        );
    }

    #[test]
    fn given_a_socket_when_copied_then_it_is_left_out() {
        // Arrange
        let (_root, src, dst) = tree();
        let _socket = std::os::unix::net::UnixListener::bind(src.join("pipe")).expect("socket");
        std::fs::write(src.join("keep"), b"x").expect("file");

        // Act
        copy(&src, &dst, &[], CopyLimits::default()).expect("copy");

        // Assert
        assert!(!dst.join("pipe").exists());
        assert!(dst.join("keep").exists());
    }

    #[test]
    fn given_a_skipped_path_when_copied_then_it_is_left_out() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::write(src.join("ward-agent"), b"agent").expect("agent");

        // Act
        copy(&src, &dst, &["/ward-agent"], CopyLimits::default()).expect("copy");

        // Assert
        assert!(!dst.join("ward-agent").exists());
    }

    #[test]
    fn given_more_bytes_than_the_cap_when_copied_then_it_is_too_large() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::write(src.join("big"), vec![0u8; 100]).expect("file");
        let limits = CopyLimits {
            max_bytes: 10,
            ..CopyLimits::default()
        };

        // Act
        let result = copy(&src, &dst, &[], limits);

        // Assert
        assert!(matches!(result, Err(CopyError::TooLarge)));
    }

    #[test]
    fn given_too_many_entries_or_depth_when_copied_then_it_is_exhausted() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::create_dir_all(src.join("a/b/c")).expect("dirs");
        std::fs::write(src.join("f"), b"x").expect("file");
        let few = CopyLimits {
            max_entries: 1,
            ..CopyLimits::default()
        };
        let shallow = CopyLimits {
            max_depth: 2,
            ..CopyLimits::default()
        };

        let second = src.parent().expect("parent").join("dst-depth");
        std::fs::create_dir_all(&second).expect("dst-depth");

        // Act
        let (by_entries, by_depth) = (
            copy(&src, &dst, &[], few),
            copy(&src, &second, &[], shallow),
        );

        // Assert
        assert!(matches!(
            by_entries,
            Err(CopyError::Backend(BackendError::ResourceExhausted(_)))
        ));
        assert!(matches!(
            by_depth,
            Err(CopyError::Backend(BackendError::ResourceExhausted(_)))
        ));
    }

    #[rstest::rstest]
    #[case::cmd_json("CMD [\"echo\",\"hi\"]", |c: &ImageConfig| c.cmd == ["echo", "hi"])]
    #[case::cmd_shell("cmd echo hi", |c: &ImageConfig| c.cmd == ["/bin/sh", "-c", "echo hi"])]
    #[case::entrypoint_clears_cmd("ENTRYPOINT [\"/run\"]", |c: &ImageConfig| c.cmd.is_empty() && c.entrypoint == ["/run"])]
    #[case::env_pairs("ENV A=1 B=2", |c: &ImageConfig| c.env == ["PATH=/bin", "A=1", "B=2"])]
    #[case::env_replaces("ENV PATH=/x", |c: &ImageConfig| c.env == ["PATH=/x"])]
    #[case::env_space_form("ENV A hello world", |c: &ImageConfig| c.env.contains(&"A=hello world".to_string()))]
    #[case::env_quoted("ENV A=\"x y\" B=2", |c: &ImageConfig| c.env == ["PATH=/bin", "A=x y", "B=2"])]
    #[case::workdir("WORKDIR /srv", |c: &ImageConfig| c.working_dir == "/srv")]
    #[case::workdir_relative("WORKDIR sub", |c: &ImageConfig| c.working_dir == "/app/sub")]
    #[case::user("USER app", |c: &ImageConfig| c.user == "app")]
    fn given_a_supported_change_when_applied_then_the_config_holds_it(
        #[case] change: &str,
        #[case] check: fn(&ImageConfig) -> bool,
    ) {
        // Arrange
        let mut config = ImageConfig {
            cmd: vec!["old".into()],
            working_dir: "/app".into(),
            env: vec!["PATH=/bin".into()],
            ..ImageConfig::default()
        };

        // Act
        apply_changes(&mut config, &[change.to_string()]).expect("apply");

        // Assert
        assert!(check(&config), "{config:?}");
    }

    #[rstest::rstest]
    #[case("EXPOSE 80")]
    #[case("LABEL a=b")]
    #[case("CMD")]
    #[case("CMD [not json")]
    #[case("ENV =x")]
    fn given_an_unsupported_change_when_applied_then_it_is_refused(#[case] change: &str) {
        // Arrange
        let mut config = ImageConfig::default();

        // Act
        let result = apply_changes(&mut config, &[change.to_string()]);

        // Assert
        assert!(
            matches!(result, Err(BackendError::InvalidArgument(_))),
            "{change}"
        );
    }

    #[test]
    fn given_setuid_and_sticky_bits_when_copied_then_only_setuid_and_setgid_are_dropped() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::write(src.join("su"), b"x").expect("file");
        std::fs::set_permissions(src.join("su"), std::fs::Permissions::from_mode(0o6755))
            .expect("mode");
        std::fs::create_dir(src.join("tmp")).expect("dir");
        std::fs::set_permissions(src.join("tmp"), std::fs::Permissions::from_mode(0o1777))
            .expect("mode");

        // Act
        copy(&src, &dst, &[], CopyLimits::default()).expect("copy");

        // Assert
        let mode = |name: &str| std::fs::metadata(dst.join(name)).unwrap().mode() & 0o7777;
        assert_eq!(mode("su"), 0o755);
        assert_eq!(mode("tmp"), 0o1777);
    }

    #[test]
    fn given_a_set_cancel_flag_when_copied_then_it_stops_as_cancelled() {
        // Arrange
        let (_root, src, dst) = tree();
        std::fs::write(src.join("f"), b"x").expect("file");

        // Act
        let result = copy_tree(
            &src,
            &dst,
            &[],
            CopyLimits::default(),
            &AtomicBool::new(true),
        );

        // Assert
        assert!(matches!(result, Err(CopyError::Cancelled)));
        assert!(!dst.join("f").exists());
    }

    #[test]
    fn given_an_unreadable_dir_or_file_when_copied_then_the_error_names_the_path() {
        for name in ["locked-dir", "locked-file"] {
            // Arrange
            let (_root, src, dst) = tree();
            if name == "locked-dir" {
                std::fs::create_dir(src.join(name)).expect("dir");
            } else {
                std::fs::write(src.join(name), b"x").expect("file");
            }
            std::fs::set_permissions(src.join(name), std::fs::Permissions::from_mode(0o000))
                .expect("mode");
            if std::fs::read_dir(src.join(name)).is_ok()
                || std::fs::File::open(src.join(name)).is_ok()
            {
                continue; // running as root: nothing is unreadable
            }

            // Act
            let error = copy(&src, &dst, &[], CopyLimits::default()).expect_err("unreadable");

            // Assert
            let CopyError::Backend(error) = error else {
                panic!("expected a backend error");
            };
            assert!(error.to_string().contains(&format!("/{name}")), "{error}");
        }
    }

    #[test]
    fn given_sandbox_settings_when_overlaid_then_they_win_over_the_image() {
        // Arrange
        let mut config = ImageConfig {
            entrypoint: vec!["/img".into()],
            env: vec!["A=1".into(), "B=1".into()],
            ..ImageConfig::default()
        };
        let spec = SandboxSpec {
            command: vec!["/run".into(), "arg".into()],
            entrypoint_len: 1,
            workdir: "/w".into(),
            ..SandboxSpec::default()
        };
        let env = [("B".to_string(), "2".to_string())].into();

        // Act
        overlay_sandbox(&mut config, &spec, &env);

        // Assert
        assert_eq!(config.entrypoint, ["/run"]);
        assert_eq!(config.cmd, ["arg"]);
        assert_eq!(config.working_dir, "/w");
        assert_eq!(config.env, ["A=1", "B=2"]);
    }
}
