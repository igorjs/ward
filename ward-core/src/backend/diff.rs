// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Host-side comparison of a sandbox rootfs against its cached image rootfs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{self, AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use super::{BackendError, Result};

/// Most entries (names seen in either tree) one comparison visits.
pub const MAX_DIFF_ENTRIES: usize = 500_000;
/// Deepest directory nesting one comparison descends.
pub const MAX_DIFF_DEPTH: usize = 64;
/// Most file bytes one comparison reads from the sandbox tree.
pub const MAX_COMPARE_BYTES: u64 = 1024 * 1024 * 1024;
/// Longest one comparison spends reading file contents.
pub const COMPARE_DEADLINE: Duration = Duration::from_secs(60);

/// Bytes read from each file per step of a content comparison.
const COMPARE_CHUNK_BYTES: usize = 64 * 1024;
/// Permission, setuid, setgid and sticky bits of a file mode.
const PERMISSION_BITS: u32 = 0o7777;
/// Flags for descending into a directory without following a link.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// Flags for reading a file: no link followed, and no blocking on a pipe.
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// How a path differs from the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One path that differs from the image. `path` is absolute inside the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsChange {
    pub kind: ChangeKind,
    pub path: PathBuf,
}

/// Caps on one comparison.
#[derive(Debug, Clone, Copy)]
pub struct DiffLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    /// Once this many bytes are read, or the deadline passes, files of equal
    /// size and mode are decided by mtime instead of content.
    pub max_compare_bytes: u64,
    pub compare_deadline: Duration,
}

impl Default for DiffLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_DIFF_ENTRIES,
            max_depth: MAX_DIFF_DEPTH,
            max_compare_bytes: MAX_COMPARE_BYTES,
            compare_deadline: COMPARE_DEADLINE,
        }
    }
}

/// Compare `current` with `base`, like `docker diff`. The guest controls
/// `current` and may change it during the walk, so both trees are walked
/// through directory fds and no link is followed: a directory swapped for a
/// link is reported as changed, and a file that vanishes or cannot be read
/// degrades that one entry instead of failing the diff.
///
/// A parent directory of any change is reported as modified. A deleted
/// directory is reported alone, an added one with everything below it, and a
/// directory replaced by a file lists its former children as deleted.
/// `skip` holds guest paths the backend writes itself, ignored in both trees.
pub fn diff_trees(
    base: &Path,
    current: &Path,
    skip: &[&str],
    limits: DiffLimits,
) -> Result<Vec<FsChange>> {
    let mut walk = Walk {
        skip,
        limits,
        started: Instant::now(),
        compared_bytes: 0,
        visited: 0,
        changes: BTreeMap::new(),
    };
    walk.compare_dir(&open_root(base)?, &open_root(current)?, Path::new(""), 1)?;
    walk.add_parent_directories();
    Ok(walk
        .changes
        .into_iter()
        .map(|(path, kind)| FsChange { kind, path })
        .collect())
}

/// Result of comparing one non-directory entry.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Same,
    Changed,
    /// The sandbox side vanished while it was being compared.
    Gone,
}

/// Result of opening a child directory.
enum Opened {
    Dir(OwnedFd),
    Gone,
    Denied,
    /// Replaced by a link or another type since it was listed.
    Swapped,
}

struct Walk<'a> {
    skip: &'a [&'a str],
    limits: DiffLimits,
    started: Instant,
    compared_bytes: u64,
    visited: usize,
    /// Keyed by absolute guest path; `PathBuf` orders by component, as
    /// `docker diff` does.
    changes: BTreeMap<PathBuf, ChangeKind>,
}

impl Walk<'_> {
    fn compare_dir(
        &mut self,
        base: &OwnedFd,
        cur: &OwnedFd,
        rel: &Path,
        depth: usize,
    ) -> Result<()> {
        let mut names = list_names(base)?;
        names.extend(list_names(cur)?);
        for name in names {
            let child = rel.join(&name);
            if self.is_skipped(&child) {
                continue;
            }
            self.visit(depth)?;
            let (before, after) = match (stat_at(base, &name), stat_at(cur, &name)) {
                (Ok(before), Ok(after)) => (before, after),
                _ => {
                    self.record(&child, ChangeKind::Modified);
                    continue;
                }
            };
            match (before, after) {
                (None, None) => {}
                (None, Some(after)) => {
                    self.record(&child, ChangeKind::Added);
                    if file_type(&after) == FileType::Directory {
                        self.record_children(cur, &name, &child, depth + 1, ChangeKind::Added)?;
                    }
                }
                (Some(_), None) => self.record(&child, ChangeKind::Deleted),
                (Some(before), Some(after)) => {
                    self.compare_entry(base, cur, &name, &child, (&before, &after), depth)?;
                }
            }
        }
        Ok(())
    }

    fn compare_entry(
        &mut self,
        base: &OwnedFd,
        cur: &OwnedFd,
        name: &OsStr,
        child: &Path,
        (before, after): (&Stat, &Stat),
        depth: usize,
    ) -> Result<()> {
        let (was_dir, is_dir) = (
            file_type(before) == FileType::Directory,
            file_type(after) == FileType::Directory,
        );
        if was_dir && is_dir {
            return self.compare_subdirs(base, cur, name, child, (before, after), depth);
        }
        let kind = match self.compare_leaf(base, cur, name, before, after) {
            Verdict::Same => return Ok(()),
            Verdict::Changed => ChangeKind::Modified,
            Verdict::Gone => ChangeKind::Deleted,
        };
        self.record(child, kind);
        if kind == ChangeKind::Modified && is_dir {
            self.record_children(cur, name, child, depth + 1, ChangeKind::Added)?;
        }
        if was_dir && !is_dir && kind == ChangeKind::Modified {
            self.record_children(base, name, child, depth + 1, ChangeKind::Deleted)?;
        }
        Ok(())
    }

    /// Directory modes are not kept by the sandbox copy, so only an owner
    /// change marks the directory itself as changed.
    fn compare_subdirs(
        &mut self,
        base: &OwnedFd,
        cur: &OwnedFd,
        name: &OsStr,
        child: &Path,
        (before, after): (&Stat, &Stat),
        depth: usize,
    ) -> Result<()> {
        if owner(before) != owner(after) {
            self.record(child, ChangeKind::Modified);
        }
        match (open_dir(base, name)?, open_dir(cur, name)?) {
            (_, Opened::Gone) => self.record(child, ChangeKind::Deleted),
            (Opened::Dir(old), Opened::Dir(new)) => {
                return self.compare_dir(&old, &new, child, depth + 1);
            }
            (Opened::Denied, _) | (_, Opened::Denied) => {}
            _ => self.record(child, ChangeKind::Modified),
        }
        Ok(())
    }

    /// Compare two entries of which at most one is a directory.
    fn compare_leaf(
        &mut self,
        base: &OwnedFd,
        cur: &OwnedFd,
        name: &OsStr,
        before: &Stat,
        after: &Stat,
    ) -> Verdict {
        let kind = file_type(after);
        if file_type(before) != kind || owner(before) != owner(after) {
            return Verdict::Changed;
        }
        match kind {
            FileType::Symlink => match (
                fs::readlinkat(base, name, Vec::new()),
                fs::readlinkat(cur, name, Vec::new()),
            ) {
                (Ok(old), Ok(new)) if old == new => Verdict::Same,
                (_, Err(Errno::NOENT)) => Verdict::Gone,
                _ => Verdict::Changed,
            },
            FileType::RegularFile => self.compare_file(base, cur, name, before, after),
            _ => verdict(before.st_rdev == after.st_rdev),
        }
    }

    fn compare_file(
        &mut self,
        base: &OwnedFd,
        cur: &OwnedFd,
        name: &OsStr,
        before: &Stat,
        after: &Stat,
    ) -> Verdict {
        if mode_bits(before) != mode_bits(after) || before.st_size != after.st_size {
            return Verdict::Changed;
        }
        let by_mtime = verdict(same_mtime(before, after));
        if self.over_budget() {
            return by_mtime;
        }
        let (old, new) = match (open_file(base, name), open_file(cur, name)) {
            (Ok(old), Ok(new)) => (old, new),
            (_, Err(Errno::NOENT)) => return Verdict::Gone,
            (Err(Errno::ACCESS), _) | (_, Err(Errno::ACCESS)) => return by_mtime,
            _ => return Verdict::Changed,
        };
        match self.same_content(old, new, by_mtime) {
            Ok(verdict) => verdict,
            Err(_) => Verdict::Changed,
        }
    }

    /// Both files are checked regular after opening, so a pipe or device
    /// swapped in after the `stat` is never read.
    fn same_content(
        &mut self,
        old: File,
        new: File,
        by_mtime: Verdict,
    ) -> std::io::Result<Verdict> {
        if !is_regular(&old)? || !is_regular(&new)? {
            return Ok(Verdict::Changed);
        }
        let (mut old, mut new) = (old, new);
        let mut left = vec![0u8; COMPARE_CHUNK_BYTES];
        let mut right = vec![0u8; COMPARE_CHUNK_BYTES];
        loop {
            if self.over_budget() {
                return Ok(by_mtime);
            }
            let read = old.read(&mut left)?;
            self.compared_bytes += read as u64;
            if read == 0 {
                return Ok(verdict(new.read(&mut right)? == 0));
            }
            if new.read_exact(&mut right[..read]).is_err() || left[..read] != right[..read] {
                return Ok(Verdict::Changed);
            }
        }
    }

    /// Record every entry below directory `name` of `parent`.
    fn record_children(
        &mut self,
        parent: &OwnedFd,
        name: &OsStr,
        rel: &Path,
        depth: usize,
        kind: ChangeKind,
    ) -> Result<()> {
        let Opened::Dir(dir) = open_dir(parent, name)? else {
            return Ok(());
        };
        for entry in list_names(&dir)? {
            let child = rel.join(&entry);
            if self.is_skipped(&child) {
                continue;
            }
            self.visit(depth)?;
            if let Ok(Some(stat)) = stat_at(&dir, &entry) {
                self.record(&child, kind);
                if file_type(&stat) == FileType::Directory {
                    self.record_children(&dir, &entry, &child, depth + 1, kind)?;
                }
            }
        }
        Ok(())
    }

    fn record(&mut self, rel: &Path, kind: ChangeKind) {
        self.changes.insert(Path::new("/").join(rel), kind);
    }

    fn add_parent_directories(&mut self) {
        let parents: Vec<PathBuf> = self
            .changes
            .keys()
            .flat_map(|path| path.ancestors().skip(1))
            .filter(|parent| *parent != Path::new("/"))
            .map(Path::to_path_buf)
            .collect();
        for parent in parents {
            self.changes.entry(parent).or_insert(ChangeKind::Modified);
        }
    }

    fn is_skipped(&self, rel: &Path) -> bool {
        let guest_path = Path::new("/").join(rel);
        self.skip
            .iter()
            .any(|skipped| Path::new(skipped) == guest_path)
    }

    /// Count an entry at nesting `depth` against the caps.
    fn visit(&mut self, depth: usize) -> Result<()> {
        if depth > self.limits.max_depth {
            return Err(BackendError::ResourceExhausted(format!(
                "the filesystem nests deeper than {} directories",
                self.limits.max_depth
            )));
        }
        self.visited += 1;
        if self.visited > self.limits.max_entries {
            return Err(BackendError::ResourceExhausted(format!(
                "the filesystem has more than {} entries to compare",
                self.limits.max_entries
            )));
        }
        Ok(())
    }

    fn over_budget(&self) -> bool {
        self.compared_bytes >= self.limits.max_compare_bytes
            || self.started.elapsed() >= self.limits.compare_deadline
    }
}

fn verdict(same: bool) -> Verdict {
    if same {
        Verdict::Same
    } else {
        Verdict::Changed
    }
}

fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

/// `st_mode` is 16 bits on macOS and 32 bits on Linux.
#[allow(clippy::useless_conversion)]
fn mode_bits(stat: &Stat) -> u32 {
    u32::from(stat.st_mode) & PERMISSION_BITS
}

fn owner(stat: &Stat) -> (u32, u32) {
    (stat.st_uid, stat.st_gid)
}

fn same_mtime(a: &Stat, b: &Stat) -> bool {
    a.st_mtime == b.st_mtime && a.st_mtime_nsec == b.st_mtime_nsec
}

fn is_regular(file: &File) -> std::io::Result<bool> {
    Ok(file_type(&fs::fstat(file)?) == FileType::RegularFile)
}

fn open_root(path: &Path) -> Result<OwnedFd> {
    Ok(fs::openat(CWD, path, DIR_FLAGS, Mode::empty()).map_err(std::io::Error::from)?)
}

/// Open directory `name` under `parent`, sorting a failure into what it means
/// for that one entry. Only unexpected errors fail the diff.
fn open_dir(parent: &OwnedFd, name: &OsStr) -> Result<Opened> {
    match fs::openat(parent, name, DIR_FLAGS, Mode::empty()) {
        Ok(fd) => Ok(Opened::Dir(fd)),
        Err(Errno::NOENT) => Ok(Opened::Gone),
        Err(Errno::ACCESS) => Ok(Opened::Denied),
        Err(Errno::LOOP | Errno::NOTDIR) => Ok(Opened::Swapped),
        Err(errno) => Err(std::io::Error::from(errno).into()),
    }
}

fn open_file(parent: &OwnedFd, name: &OsStr) -> std::result::Result<File, Errno> {
    fs::openat(parent, name, FILE_FLAGS, Mode::empty()).map(File::from)
}

/// Metadata of `name` itself, never of a link's target. `None` if it is gone.
fn stat_at(dir: &OwnedFd, name: &OsStr) -> std::result::Result<Option<Stat>, Errno> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::NOENT) => Ok(None),
        Err(errno) => Err(errno),
    }
}

/// Names in `dir`, read up front so no directory stream stays open while the
/// walk descends.
fn list_names(dir: &OwnedFd) -> Result<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for entry in Dir::read_from(dir).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.insert(OsStr::from_bytes(name).to_os_string());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::ChangeKind::{Added, Deleted, Modified};
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const AGENT: &str = "/ward-agent";
    const NO_ACCESS: u32 = 0o000;

    struct Trees {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        current: PathBuf,
    }

    fn trees() -> Trees {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("base");
        let current = tmp.path().join("current");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        Trees {
            _tmp: tmp,
            base,
            current,
        }
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn run(t: &Trees) -> Vec<(ChangeKind, String)> {
        run_with(t, DiffLimits::default()).expect("diff")
    }

    fn run_with(t: &Trees, limits: DiffLimits) -> Result<Vec<(ChangeKind, String)>> {
        Ok(diff_trees(&t.base, &t.current, &[AGENT], limits)?
            .into_iter()
            .map(|c| (c.kind, c.path.to_string_lossy().into_owned()))
            .collect())
    }

    fn entry(kind: ChangeKind, path: &str) -> (ChangeKind, String) {
        (kind, path.to_string())
    }

    #[test]
    fn given_added_changed_and_deleted_files_when_diffed_then_each_is_reported() {
        // Arrange
        let t = trees();
        write(&t.base, "changed", b"aaa");
        write(&t.current, "changed", b"bbb");
        write(&t.base, "deleted", b"gone");
        write(&t.base, "same", b"same");
        write(&t.current, "same", b"same");
        write(&t.current, "added", b"new");

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Added, "/added"),
                entry(Modified, "/changed"),
                entry(Deleted, "/deleted"),
            ]
        );
    }

    #[test]
    fn given_size_or_mode_change_when_diffed_then_the_file_is_modified() {
        // Arrange
        let t = trees();
        write(&t.base, "longer", b"ab");
        write(&t.current, "longer", b"abc");
        write(&t.base, "mode", b"x");
        write(&t.current, "mode", b"x");
        std::fs::set_permissions(t.current.join("mode"), PermissionsExt::from_mode(0o700)).unwrap();

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![entry(Modified, "/longer"), entry(Modified, "/mode")]
        );
    }

    #[test]
    fn given_changes_below_a_directory_when_diffed_then_the_parents_are_modified() {
        // Arrange
        let t = trees();
        write(&t.base, "a/b/old", b"x");
        write(&t.current, "a/b/new", b"y");
        write(&t.base, "keep/f", b"z");
        write(&t.current, "keep/f", b"z");

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Modified, "/a"),
                entry(Modified, "/a/b"),
                entry(Added, "/a/b/new"),
                entry(Deleted, "/a/b/old"),
            ]
        );
    }

    #[test]
    fn given_new_and_deleted_directories_when_diffed_then_new_lists_children_and_deleted_does_not()
    {
        // Arrange
        let t = trees();
        write(&t.current, "newdir/f", b"x");
        write(&t.base, "olddir/f", b"x");

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Added, "/newdir"),
                entry(Added, "/newdir/f"),
                entry(Deleted, "/olddir"),
            ]
        );
    }

    #[test]
    fn given_siblings_sharing_a_prefix_when_diffed_then_paths_sort_by_component() {
        // Arrange
        let t = trees();
        write(&t.current, "a/b", b"x");
        write(&t.current, "a-b", b"x");

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Added, "/a"),
                entry(Added, "/a/b"),
                entry(Added, "/a-b"),
            ]
        );
    }

    #[test]
    fn given_only_boot_files_and_unreadable_or_sticky_entries_when_diffed_then_empty() {
        // Arrange: the sandbox copy loses sticky bits; an unreadable file with
        // equal mode, size and mtime is unchanged.
        let t = trees();
        write(&t.base, "etc/shadow", b"secret");
        write(&t.current, "etc/shadow", b"secret");
        let stamp = std::fs::metadata(t.base.join("etc/shadow"))
            .unwrap()
            .modified()
            .unwrap();
        File::options()
            .write(true)
            .open(t.current.join("etc/shadow"))
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        std::fs::set_permissions(
            t.base.join("etc/shadow"),
            PermissionsExt::from_mode(NO_ACCESS),
        )
        .unwrap();
        std::fs::set_permissions(
            t.current.join("etc/shadow"),
            PermissionsExt::from_mode(NO_ACCESS),
        )
        .unwrap();
        std::fs::create_dir_all(t.base.join("tmp")).unwrap();
        std::fs::create_dir_all(t.current.join("tmp")).unwrap();
        std::fs::set_permissions(t.base.join("tmp"), PermissionsExt::from_mode(0o1777)).unwrap();
        write(&t.current, "ward-agent", b"agent binary");

        // Act
        let changes = run(&t);

        // Assert
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn given_a_symlink_to_a_host_file_when_diffed_then_it_is_a_link_and_the_file_is_not_read() {
        // Arrange: a host file nobody can read, so any read would fail or leak.
        let t = trees();
        let host = t._tmp.path().join("host-secret");
        std::fs::write(&host, b"host bytes").unwrap();
        std::fs::set_permissions(&host, PermissionsExt::from_mode(NO_ACCESS)).unwrap();
        symlink(&host, t.current.join("added-link")).unwrap();
        write(&t.base, "was-file", b"host bytes");
        symlink(&host, t.current.join("was-file")).unwrap();
        symlink(&host, t.base.join("same-link")).unwrap();
        symlink(&host, t.current.join("same-link")).unwrap();
        symlink("/one", t.base.join("moved-link")).unwrap();
        symlink("/two", t.current.join("moved-link")).unwrap();

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Added, "/added-link"),
                entry(Modified, "/moved-link"),
                entry(Modified, "/was-file"),
            ]
        );
    }

    #[rstest::rstest]
    #[case::below_cap(2, 3, true)]
    #[case::at_cap(3, 3, true)]
    #[case::above_cap(4, 3, false)]
    fn given_an_entry_cap_when_diffed_then_only_a_walk_over_the_cap_fails(
        #[case] files: usize,
        #[case] cap: usize,
        #[case] ok: bool,
    ) {
        // Arrange
        let t = trees();
        for i in 0..files {
            write(&t.current, &format!("f{i}"), b"x");
        }
        let limits = DiffLimits {
            max_entries: cap,
            ..DiffLimits::default()
        };

        // Act
        let result = run_with(&t, limits);

        // Assert
        assert_eq!(result.is_ok(), ok);
    }

    #[rstest::rstest]
    #[case::below_cap(1, true)]
    #[case::at_cap(2, true)]
    #[case::above_cap(3, false)]
    fn given_a_depth_cap_when_diffed_then_only_a_deeper_walk_fails(
        #[case] depth: usize,
        #[case] ok: bool,
    ) {
        // Arrange: `depth` nested directories.
        let t = trees();
        let nested = (0..depth).map(|i| format!("d{i}")).collect::<Vec<_>>();
        std::fs::create_dir_all(t.current.join(nested.join("/"))).unwrap();
        let limits = DiffLimits {
            max_depth: 2,
            ..DiffLimits::default()
        };

        // Act
        let result = run_with(&t, limits);

        // Assert
        assert_eq!(result.is_ok(), ok);
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn given_a_directory_swapped_for_a_link_to_a_sentinel_when_diffed_then_the_link_is_not_followed()
     {
        // Arrange
        let t = trees();
        write(&t.base, "d/f", b"x");
        let sentinel = t._tmp.path().join("sentinel");
        write(&sentinel, "secret", b"host");
        symlink(&sentinel, t.current.join("d")).unwrap();

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(changes, vec![entry(Modified, "/d"), entry(Deleted, "/d/f")]);
    }

    #[test]
    fn given_a_link_where_a_directory_was_listed_when_opened_then_it_is_swapped_not_followed() {
        // Arrange
        let t = trees();
        let sentinel = t._tmp.path().join("sentinel");
        std::fs::create_dir_all(&sentinel).unwrap();
        symlink(&sentinel, t.current.join("d")).unwrap();
        let root = open_root(&t.current).unwrap();

        // Act
        let opened = open_dir(&root, OsStr::new("d")).unwrap();

        // Assert
        assert!(matches!(opened, Opened::Swapped));
    }

    #[test]
    fn given_a_file_removed_after_it_was_listed_when_compared_then_it_is_gone_not_an_error() {
        // Arrange
        let t = trees();
        write(&t.base, "f", b"x");
        write(&t.current, "f", b"x");
        let (base, cur) = (open_root(&t.base).unwrap(), open_root(&t.current).unwrap());
        let name = OsStr::new("f");
        let (before, after) = (
            stat_at(&base, name).unwrap().unwrap(),
            stat_at(&cur, name).unwrap().unwrap(),
        );
        std::fs::remove_file(t.current.join("f")).unwrap();
        let mut walk = Walk {
            skip: &[],
            limits: DiffLimits::default(),
            started: Instant::now(),
            compared_bytes: 0,
            visited: 0,
            changes: BTreeMap::new(),
        };

        // Act
        let verdict = walk.compare_leaf(&base, &cur, name, &before, &after);

        // Assert
        assert_eq!(verdict, Verdict::Gone);
    }

    #[test]
    fn given_a_directory_the_daemon_cannot_open_when_diffed_then_it_is_skipped() {
        // Arrange
        let t = trees();
        for root in [&t.base, &t.current] {
            write(root, "locked/f", b"x");
            std::fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(NO_ACCESS))
                .unwrap();
        }

        // Act
        let changes = run(&t);

        // Assert
        for root in [&t.base, &t.current] {
            std::fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn given_special_files_when_diffed_then_type_decides_and_a_fifo_is_never_opened() {
        // Arrange
        let t = trees();
        mkfifo(&t.base.join("same"));
        mkfifo(&t.current.join("same"));
        mkfifo(&t.base.join("now-file"));
        write(&t.current, "now-file", b"x");
        write(&t.base, "now-fifo", b"x");
        mkfifo(&t.current.join("now-fifo"));

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![entry(Modified, "/now-fifo"), entry(Modified, "/now-file")]
        );
    }

    #[test]
    fn given_replaced_directories_and_files_when_diffed_then_they_are_changed_with_their_children()
    {
        // Arrange
        let t = trees();
        write(&t.base, "was-dir/child", b"x");
        write(&t.current, "was-dir", b"now a file");
        write(&t.base, "was-file", b"x");
        write(&t.current, "was-file/child", b"y");

        // Act
        let changes = run(&t);

        // Assert
        assert_eq!(
            changes,
            vec![
                entry(Modified, "/was-dir"),
                entry(Deleted, "/was-dir/child"),
                entry(Modified, "/was-file"),
                entry(Added, "/was-file/child"),
            ]
        );
    }

    #[test]
    fn given_the_compare_budget_is_spent_when_diffed_then_equal_size_files_are_decided_by_mtime() {
        // Arrange
        let t = trees();
        write(&t.base, "a", b"aaa");
        write(&t.current, "a", b"bbb");
        let stamp = std::fs::metadata(t.base.join("a"))
            .unwrap()
            .modified()
            .unwrap();
        File::options()
            .write(true)
            .open(t.current.join("a"))
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        let spent = DiffLimits {
            max_compare_bytes: 0,
            ..DiffLimits::default()
        };

        // Act
        let spent_result = run_with(&t, spent).unwrap();
        let full_result = run(&t);

        // Assert
        assert!(spent_result.is_empty(), "{spent_result:?}");
        assert_eq!(full_result, vec![entry(Modified, "/a")]);
    }
}
