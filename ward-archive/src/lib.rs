// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Safe tar unpacking shared by snapshots and file copy.
//!
//! Every archive is untrusted. Both policies reject absolute paths, `..`,
//! hardlinks that leave the root and writes through symlinks, and both
//! enforce the [`Limits`]. They differ only on special files and mode bits:
//! [`Policy::Snapshot`] restores a guest rootfs, so it strips setuid and
//! setgid bits, keeps the sticky bit and whatever else the image holds.
//! [`Policy::Copy`] moves user files, so it rejects the setuid and setgid
//! bits and every entry that is not a plain file, directory or link.
//!
//! Oversized GNU long-name, long-link and PAX headers are refused while the
//! stream is read, before the tar crate buffers their bodies.

use std::cell::RefCell;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use rustix::fs::{self, Mode, OFlags};
use rustix::io::Errno;

/// Most entries an archive may hold.
pub const MAX_ENTRIES: u64 = 100_000;
/// Most bytes one file may hold (4 GiB).
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Most bytes all files together may hold (8 GiB).
pub const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Longest path Linux accepts (`PATH_MAX`).
const LINUX_PATH_MAX: usize = 4096;
/// Longest entry path in bytes.
pub const MAX_PATH_LEN: usize = LINUX_PATH_MAX;
/// Deepest entry path in components.
pub const MAX_DEPTH: usize = 64;
/// Largest GNU long-name, long-link or PAX header body (1 MiB).
pub const MAX_EXTENDED_HEADER_BYTES: u64 = 1024 * 1024;

/// Snapshot restores read a whole guest rootfs, so their caps are far above
/// the copy caps while still bounded.
pub const SNAPSHOT_MAX_ENTRIES: u64 = 2_000_000;
/// Most bytes a restored rootfs may hold (64 GiB).
pub const SNAPSHOT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// A rootfs file may be as large as the whole rootfs.
pub const SNAPSHOT_MAX_FILE_BYTES: u64 = SNAPSHOT_MAX_TOTAL_BYTES;
/// Deepest restored path: far past what `PATH_MAX` lets a guest create.
pub const SNAPSHOT_MAX_DEPTH: usize = 4096;

/// Mode bits that make a file run with another user or group.
const SETID_MODE_BITS: u32 = 0o6000;
/// Mode bits a snapshot restore keeps: permissions plus the sticky bit.
const RESTORED_MODE_BITS: u32 = 0o1777;

/// Tar block size, the unit of headers and padded bodies.
const BLOCK_SIZE: usize = 512;
/// Offset of the "extended sparse headers follow" flag in an extended sparse
/// header block.
const EXT_SPARSE_IS_EXTENDED_OFFSET: usize = 504;
/// Header bytes that hold the checksum, which count as spaces in its sum.
const CKSUM_FIELD: std::ops::Range<usize> = 148..156;
/// Value of each checksum byte while the checksum is computed.
const CKSUM_FILL: u32 = b' ' as u32;
/// PAX key that overrides the entry size.
const PAX_SIZE_KEY: &[u8] = b"size";

/// How strictly an archive is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Rootfs restore: strip setuid and setgid bits, allow special files.
    Snapshot,
    /// File copy: reject setuid and setgid bits and special files.
    Copy,
}

/// Caps on what one archive may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_path_len: usize,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: MAX_ENTRIES,
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_path_len: MAX_PATH_LEN,
            max_depth: MAX_DEPTH,
        }
    }
}

impl Limits {
    /// Caps for restoring a rootfs snapshot.
    pub fn snapshot() -> Self {
        Self {
            max_entries: SNAPSHOT_MAX_ENTRIES,
            max_file_bytes: SNAPSHOT_MAX_FILE_BYTES,
            max_total_bytes: SNAPSHOT_MAX_TOTAL_BYTES,
            max_path_len: LINUX_PATH_MAX,
            max_depth: SNAPSHOT_MAX_DEPTH,
        }
    }
}

/// Why an archive was refused.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive entry has an unsafe path: {0}")]
    UnsafePath(String),
    #[error("archive entry has a link target outside the root: {0}")]
    UnsafeLink(String),
    #[error("archive entry type is not allowed by the copy policy: {0}")]
    ForbiddenType(String),
    #[error("archive entry sets the setuid or setgid bit: {0}")]
    SetIdMode(String),
    #[error("archive header is not allowed: {0}")]
    ForbiddenHeader(&'static str),
    #[error("archive passes the {what} limit of {limit}")]
    LimitExceeded { what: &'static str, limit: u64 },
    #[error("cannot unpack archive entry {path}: {source}")]
    Unpack {
        path: String,
        source: std::io::Error,
    },
    #[error("archive io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for archive operations.
pub type Result<T> = std::result::Result<T, ArchiveError>;

/// Running totals checked against [`Limits`].
#[derive(Default)]
struct Totals {
    entries: u64,
    bytes: u64,
}

/// Unpack `reader` into the existing directory `dest`.
///
/// Entries before a failing one stay on disk, so callers that need all or
/// nothing unpack into a staging directory.
pub fn unpack<R: Read>(reader: R, dest: &Path, policy: Policy, limits: &Limits) -> Result<()> {
    scan(reader, policy, limits, Some(dest))
}

fn os_error(errno: Errno) -> ArchiveError {
    ArchiveError::Io(errno.into())
}

/// Where the guard leaves the reason it refused the stream.
type Verdict = Rc<RefCell<Option<ArchiveError>>>;

fn scan<R: Read>(reader: R, policy: Policy, limits: &Limits, dest: Option<&Path>) -> Result<()> {
    let verdict = Verdict::default();
    let guard = HeaderGuard::new(reader, policy == Policy::Copy, Rc::clone(&verdict));
    let blame = |e: io::Error| verdict.borrow_mut().take().unwrap_or(ArchiveError::Io(e));
    let mut archive = tar::Archive::new(guard);
    // Overwrite is safe: `unpack_in` refuses paths that leave `dest`. The
    // header mode is not applied by tar (`set_preserve_permissions(false)`
    // leaves the process umask in charge), so a snapshot restore sets the
    // modes it keeps itself.
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    let mut totals = Totals::default();
    let mut dir_modes: Vec<DirMode> = Vec::new();
    for entry in archive.entries().map_err(blame)? {
        let mut entry = entry.map_err(blame)?;
        let path = entry.path()?.into_owned();
        let shown = path.display().to_string();
        check_entry(&entry, &path, policy, limits, &mut totals)?;
        let Some(dest) = dest else { continue };
        let kind = entry.header().entry_type();
        let mode = entry.header().mode()? & RESTORED_MODE_BITS;
        if kind == tar::EntryType::Link {
            check_hardlink_inside(&entry, dest)?;
        }
        let written = entry
            .unpack_in(dest)
            .map_err(|source| ArchiveError::Unpack {
                path: shown.clone(),
                source,
            })?;
        if !written {
            return Err(ArchiveError::UnsafePath(shown));
        }
        if policy == Policy::Snapshot {
            let target = dest.join(&path);
            match kind {
                tar::EntryType::Regular | tar::EntryType::Continuous => set_mode(&target, mode)?,
                // Applied last so a read-only directory cannot block its children.
                tar::EntryType::Directory => dir_modes.push(DirMode::record(target, mode)?),
                _ => {}
            }
        }
    }
    for dir in dir_modes.into_iter().rev() {
        dir.apply()?;
    }
    Ok(())
}

/// A directory whose mode is applied after its children are written. It is
/// pinned by inode, because a later symlink entry can re-point a parent of
/// its path at a host directory.
struct DirMode {
    path: PathBuf,
    mode: u32,
    dev: u64,
    ino: u64,
}

impl DirMode {
    fn record(path: PathBuf, mode: u32) -> Result<Self> {
        let stat = fs::fstat(open_dir_nofollow(&path)?).map_err(os_error)?;
        Ok(Self {
            path,
            mode,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
        })
    }

    fn apply(self) -> Result<()> {
        let fd = open_dir_nofollow(&self.path)?;
        let stat = fs::fstat(&fd).map_err(os_error)?;
        if stat.st_dev as u64 != self.dev || stat.st_ino as u64 != self.ino {
            return Err(ArchiveError::UnsafePath(self.path.display().to_string()));
        }
        let mode = Mode::from_bits_truncate(self.mode.try_into().unwrap_or_default());
        fs::fchmod(&fd, mode).map_err(os_error)
    }
}

fn open_dir_nofollow(path: &Path) -> Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    fs::open(path, flags, Mode::empty()).map_err(os_error)
}

#[cfg(unix)]
fn set_mode(target: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_target: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Reads tar blocks as they pass and refuses headers whose bodies the tar
/// crate would buffer whole, before it gets the chance to.
struct HeaderGuard<R> {
    inner: R,
    reject_sparse: bool,
    verdict: Verdict,
    failed: bool,
    state: GuardState,
    block: [u8; BLOCK_SIZE],
    filled: usize,
    pax: Vec<u8>,
}

enum GuardState {
    /// Expecting a header block.
    Header,
    /// Expecting extended sparse blocks, then a body of `body` bytes.
    SparseExtension { body: u64 },
    /// Passing a body: `left` bytes including padding, of which the first
    /// `data_left` are data, collected when `pax` is set.
    Body {
        left: u64,
        data_left: u64,
        pax: bool,
    },
}

impl<R> HeaderGuard<R> {
    fn new(inner: R, reject_sparse: bool, verdict: Verdict) -> Self {
        Self {
            inner,
            reject_sparse,
            verdict,
            failed: false,
            state: GuardState::Header,
            block: [0; BLOCK_SIZE],
            filled: 0,
            pax: Vec::new(),
        }
    }

    fn observe(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            match &mut self.state {
                GuardState::Header | GuardState::SparseExtension { .. } => {
                    let take = (BLOCK_SIZE - self.filled).min(bytes.len());
                    self.block[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
                    self.filled += take;
                    bytes = &bytes[take..];
                    if self.filled == BLOCK_SIZE {
                        self.filled = 0;
                        self.finish_block()?;
                    }
                }
                GuardState::Body {
                    left,
                    data_left,
                    pax,
                } => {
                    let take = (*left).min(bytes.len() as u64) as usize;
                    if *pax {
                        let data = take.min(*data_left as usize);
                        self.pax.extend_from_slice(&bytes[..data]);
                        *data_left -= data as u64;
                    }
                    *left -= take as u64;
                    bytes = &bytes[take..];
                    if *left == 0 {
                        let was_pax = *pax;
                        self.state = GuardState::Header;
                        if was_pax {
                            self.finish_pax()?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish_block(&mut self) -> Result<()> {
        if let GuardState::SparseExtension { body } = self.state {
            if self.block[EXT_SPARSE_IS_EXTENDED_OFFSET] != 1 {
                self.start_body(body, false);
            }
            return Ok(());
        }
        if self.block.iter().all(|&b| b == 0) {
            return Ok(());
        }
        // The tar crate's own parsers decide the size, type and checksum, so
        // the guard cannot disagree with it about where a body ends.
        let block = self.block;
        let header = tar::Header::from_byte_slice(&block);
        if !checksum_matches(header, &block) {
            return Err(ArchiveError::ForbiddenHeader("invalid header checksum"));
        }
        let size = header
            .entry_size()
            .map_err(|_| ArchiveError::ForbiddenHeader("unreadable size field"))?;
        let kind = header.entry_type();
        if kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions()
        {
            if size > MAX_EXTENDED_HEADER_BYTES {
                return Err(limit("extended header size", MAX_EXTENDED_HEADER_BYTES));
            }
            let is_pax = kind.is_pax_local_extensions() || kind.is_pax_global_extensions();
            self.start_body(size, is_pax);
        } else if kind.is_gnu_sparse() {
            if self.reject_sparse {
                return Err(ArchiveError::ForbiddenType("GNU sparse file".into()));
            }
            if header.as_gnu().is_some_and(|gnu| gnu.is_extended()) {
                self.state = GuardState::SparseExtension { body: size };
            } else {
                self.start_body(size, false);
            }
        } else {
            self.start_body(size, false);
        }
        Ok(())
    }

    fn start_body(&mut self, size: u64, pax: bool) {
        let padded = size
            .div_ceil(BLOCK_SIZE as u64)
            .saturating_mul(BLOCK_SIZE as u64);
        self.state = if padded == 0 {
            GuardState::Header
        } else {
            GuardState::Body {
                left: padded,
                data_left: size,
                pax,
            }
        };
        self.pax.clear();
    }

    /// A PAX `size` record would make the tar crate read a different body
    /// length than the guard skipped, hiding later headers from it.
    fn finish_pax(&mut self) -> Result<()> {
        for record in tar::PaxExtensions::new(&self.pax) {
            let record =
                record.map_err(|_| ArchiveError::ForbiddenHeader("malformed PAX record"))?;
            if record.key_bytes() == PAX_SIZE_KEY {
                return Err(ArchiveError::ForbiddenHeader("PAX size override"));
            }
        }
        self.pax.clear();
        Ok(())
    }
}

impl<R: Read> Read for HeaderGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::other("archive refused"));
        }
        let n = self.inner.read(buf)?;
        if let Err(refusal) = self.observe(&buf[..n]) {
            self.failed = true;
            *self.verdict.borrow_mut() = Some(refusal);
            return Err(io::Error::other("archive refused"));
        }
        Ok(n)
    }
}

/// Same checksum rule as the tar crate: the sum of every header byte with
/// the checksum field counted as spaces.
fn checksum_matches(header: &tar::Header, block: &[u8; BLOCK_SIZE]) -> bool {
    let sum: u32 = block
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if CKSUM_FIELD.contains(&i) {
                CKSUM_FILL
            } else {
                u32::from(b)
            }
        })
        .sum();
    header.cksum().is_ok_and(|stored| stored == sum)
}

fn check_entry<R: Read>(
    entry: &tar::Entry<'_, R>,
    path: &Path,
    policy: Policy,
    limits: &Limits,
    totals: &mut Totals,
) -> Result<()> {
    let shown = || path.display().to_string();

    totals.entries += 1;
    if totals.entries > limits.max_entries {
        return Err(limit("entry count", limits.max_entries));
    }

    if path.as_os_str().len() > limits.max_path_len {
        return Err(limit("path length", limits.max_path_len as u64));
    }
    if path.components().any(|c| !is_contained(c)) {
        return Err(ArchiveError::UnsafePath(shown()));
    }
    let depth = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if depth > limits.max_depth {
        return Err(limit("path depth", limits.max_depth as u64));
    }

    let size = entry.size();
    if size > limits.max_file_bytes {
        return Err(limit("file size", limits.max_file_bytes));
    }
    totals.bytes = totals.bytes.saturating_add(size);
    if totals.bytes > limits.max_total_bytes {
        return Err(limit("total size", limits.max_total_bytes));
    }

    // Symlink targets resolve inside the guest, where absolute and `..`
    // targets are normal (`var/run -> /run`). Writes through a symlink
    // are stopped by `unpack_in`. Hardlinks resolve on the host.
    if entry.header().entry_type() == tar::EntryType::Link {
        let target = entry
            .link_name()?
            .ok_or_else(|| ArchiveError::UnsafeLink(shown()))?;
        if target.components().any(|c| !is_contained(c)) {
            return Err(ArchiveError::UnsafeLink(shown()));
        }
    }

    if policy == Policy::Copy {
        if !is_plain_type(entry.header().entry_type()) {
            return Err(ArchiveError::ForbiddenType(shown()));
        }
        if entry.header().mode()? & SETID_MODE_BITS != 0 {
            return Err(ArchiveError::SetIdMode(shown()));
        }
    }
    Ok(())
}

fn limit(what: &'static str, limit: u64) -> ArchiveError {
    ArchiveError::LimitExceeded { what, limit }
}

/// Only relative, forward-moving components stay inside the root.
fn is_contained(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(_) | Component::CurDir)
}

fn is_plain_type(kind: tar::EntryType) -> bool {
    matches!(
        kind,
        tar::EntryType::Regular
            | tar::EntryType::Continuous
            | tar::EntryType::Directory
            | tar::EntryType::Symlink
            | tar::EntryType::Link
            | tar::EntryType::XGlobalHeader
    )
}

/// A hardlink target that crosses an earlier symlink would link a host file
/// into the root, so its parent must resolve inside `dest`.
fn check_hardlink_inside<R: Read>(entry: &tar::Entry<'_, R>, dest: &Path) -> Result<()> {
    let Some(target) = entry.link_name()? else {
        return Ok(());
    };
    let outside = || ArchiveError::UnsafeLink(target.display().to_string());
    let root = dest.canonicalize()?;
    let parent = dest.join(&target);
    let Some(parent) = parent.parent() else {
        return Err(outside());
    };
    // A missing parent is left to `unpack_in`, which fails the link itself.
    match parent.canonicalize() {
        Ok(resolved) if !resolved.starts_with(&root) => Err(outside()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::io::Cursor;

    /// Small cap used by the table tests.
    const CAP: usize = 3;

    /// A header whose path bytes are written raw, because `set_path`
    /// refuses the hostile names these tests need.
    fn header(name: &str, kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        let field = &mut h.as_gnu_mut().unwrap().name;
        field[..name.len()].copy_from_slice(name.as_bytes());
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_cksum();
        h
    }

    fn link_header(name: &str, kind: tar::EntryType, target: &str) -> tar::Header {
        let mut h = header(name, kind, 0o777, 0);
        let field = &mut h.as_gnu_mut().unwrap().linkname;
        field[..target.len()].copy_from_slice(target.as_bytes());
        h.set_cksum();
        h
    }

    fn build(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (h, data) in entries {
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn file(name: &str, size: usize) -> (tar::Header, Vec<u8>) {
        let h = header(name, tar::EntryType::Regular, 0o644, size as u64);
        (h, vec![b'x'; size])
    }

    fn run(tar: &[u8], policy: Policy, limits: &Limits) -> (tempfile::TempDir, Result<()>) {
        let dest = tempfile::tempdir().unwrap();
        let result = unpack(Cursor::new(tar), dest.path(), policy, limits);
        (dest, result)
    }

    #[derive(Debug, Clone, Copy)]
    enum Cap {
        Entries,
        FileBytes,
        TotalBytes,
        PathLen,
        Depth,
    }

    /// An archive that holds exactly `units` of the capped quantity.
    fn archive_with(cap: Cap, units: usize) -> Vec<u8> {
        match cap {
            Cap::Entries => build((0..units).map(|i| file(&format!("f{i}"), 0)).collect()),
            Cap::FileBytes => build(vec![file("f", units)]),
            Cap::TotalBytes => build((0..units).map(|i| file(&format!("f{i}"), 1)).collect()),
            Cap::PathLen => build(vec![file(&"n".repeat(units), 0)]),
            Cap::Depth => build(vec![file(&vec!["d"; units].join("/"), 0)]),
        }
    }

    /// Limits where only `cap` is tight, so other quantities never trip.
    fn limits_for(cap: Cap) -> Limits {
        let loose = Limits {
            max_entries: 100,
            max_file_bytes: 100,
            max_total_bytes: 100,
            max_path_len: 100,
            max_depth: 100,
        };
        let n = CAP;
        match cap {
            Cap::Entries => Limits {
                max_entries: n as u64,
                ..loose
            },
            Cap::FileBytes => Limits {
                max_file_bytes: n as u64,
                ..loose
            },
            Cap::TotalBytes => Limits {
                max_total_bytes: n as u64,
                ..loose
            },
            Cap::PathLen => Limits {
                max_path_len: n,
                ..loose
            },
            Cap::Depth => Limits {
                max_depth: n,
                ..loose
            },
        }
    }

    #[rstest]
    fn given_each_cap_when_unpacking_then_limit_minus_one_and_limit_pass_and_limit_plus_one_fails(
        #[values(
            Cap::Entries,
            Cap::FileBytes,
            Cap::TotalBytes,
            Cap::PathLen,
            Cap::Depth
        )]
        cap: Cap,
        #[values(Policy::Snapshot, Policy::Copy)] policy: Policy,
    ) {
        // Arrange
        let limits = limits_for(cap);

        // Act
        let below = run(&archive_with(cap, CAP - 1), policy, &limits).1;
        let at = run(&archive_with(cap, CAP), policy, &limits).1;
        let above = run(&archive_with(cap, CAP + 1), policy, &limits).1;

        // Assert
        assert!(below.is_ok(), "{cap:?} at limit-1: {below:?}");
        assert!(at.is_ok(), "{cap:?} at limit: {at:?}");
        assert!(
            matches!(above, Err(ArchiveError::LimitExceeded { .. })),
            "{cap:?} at limit+1: {above:?}"
        );
    }

    #[test]
    fn given_default_limits_when_read_then_they_match_the_named_constants() {
        // Arrange / Act
        let limits = Limits::default();

        // Assert
        assert_eq!(limits.max_entries, MAX_ENTRIES);
        assert_eq!(limits.max_total_bytes, MAX_TOTAL_BYTES);
        assert_eq!(limits.max_depth, MAX_DEPTH);
    }

    #[test]
    fn given_one_more_entry_than_the_copy_cap_when_restored_then_snapshot_limits_accept_and_copy_rejects()
     {
        // Arrange
        let count = MAX_ENTRIES as usize + 1;
        let tar = build((0..count).map(|i| file(&format!("f{i}"), 0)).collect());

        // Act
        let snapshot = run(&tar, Policy::Snapshot, &Limits::snapshot()).1;
        let copy = run(&tar, Policy::Copy, &Limits::default()).1;

        // Assert
        snapshot.expect("snapshot limits accept copy cap + 1");
        assert!(matches!(copy, Err(ArchiveError::LimitExceeded { .. })));
    }

    #[derive(Debug, Clone, Copy)]
    enum Hostile {
        ParentDir,
        Absolute,
        SymlinkThenWrite,
        HardlinkParent,
        HardlinkAbsolute,
        HardlinkThroughSymlink,
    }

    fn hostile_tar(kind: Hostile, outside: &Path) -> Vec<u8> {
        let out = outside.to_str().unwrap();
        let sym = |name: &str| link_header(name, tar::EntryType::Symlink, out);
        match kind {
            Hostile::ParentDir => build(vec![file("../evil", 1)]),
            Hostile::Absolute => build(vec![file("/tmp/evil", 1)]),
            Hostile::SymlinkThenWrite => build(vec![(sym("etc"), vec![]), file("etc/evil", 1)]),
            Hostile::HardlinkParent => build(vec![(
                link_header("l", tar::EntryType::Link, "../outside/secret"),
                vec![],
            )]),
            Hostile::HardlinkAbsolute => build(vec![(
                link_header("l", tar::EntryType::Link, "/etc/passwd"),
                vec![],
            )]),
            Hostile::HardlinkThroughSymlink => build(vec![
                (sym("etc"), vec![]),
                (link_header("l", tar::EntryType::Link, "etc/secret"), vec![]),
            ]),
        }
    }

    #[rstest]
    fn given_a_hostile_entry_when_unpacking_then_it_is_rejected_and_nothing_escapes(
        #[values(
            Hostile::ParentDir,
            Hostile::Absolute,
            Hostile::SymlinkThenWrite,
            Hostile::HardlinkParent,
            Hostile::HardlinkAbsolute,
            Hostile::HardlinkThroughSymlink
        )]
        kind: Hostile,
        #[values(Policy::Snapshot, Policy::Copy)] policy: Policy,
    ) {
        // Arrange
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let tar = hostile_tar(kind, &outside);

        // Act
        let result = unpack(Cursor::new(tar), &root, policy, &Limits::default());

        // Assert
        assert!(result.is_err(), "{kind:?} under {policy:?} was accepted");
        assert!(!outside.join("evil").exists(), "{kind:?} wrote outside");
        assert!(!root.join("l").exists(), "{kind:?} linked a host file");
    }

    #[rstest]
    #[case::char_device(tar::EntryType::Char)]
    #[case::block_device(tar::EntryType::Block)]
    #[case::fifo(tar::EntryType::Fifo)]
    fn given_a_special_file_when_unpacking_with_the_copy_policy_then_it_is_rejected(
        #[case] kind: tar::EntryType,
    ) {
        // Arrange
        let tar = build(vec![(header("node", kind, 0o644, 0), vec![])]);

        // Act
        let (dest, result) = run(&tar, Policy::Copy, &Limits::default());

        // Assert
        assert!(matches!(result, Err(ArchiveError::ForbiddenType(_))));
        assert!(!dest.path().join("node").exists());
    }

    #[cfg(unix)]
    #[test]
    fn given_a_setuid_file_when_unpacked_then_snapshot_strips_the_bit_and_copy_rejects_it() {
        // Arrange
        use std::os::unix::fs::PermissionsExt;
        let tar = build(vec![(
            header("su", tar::EntryType::Regular, 0o6755, 1),
            vec![b'x'],
        )]);

        // Act
        let (snapshot_dir, snapshot) = run(&tar, Policy::Snapshot, &Limits::default());
        let (copy_dir, copy) = run(&tar, Policy::Copy, &Limits::default());

        // Assert
        snapshot.expect("snapshot policy accepts setuid files");
        let mode = std::fs::metadata(snapshot_dir.path().join("su"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & SETID_MODE_BITS, 0, "setuid or setgid bit survived");
        assert!(matches!(copy, Err(ArchiveError::SetIdMode(_))));
        assert!(!copy_dir.path().join("su").exists());
    }

    #[test]
    fn given_a_hostile_archive_when_scanned_without_a_destination_then_it_is_rejected_and_a_clean_one_passes()
     {
        // Arrange
        let clean = build(vec![file("a/b", 2)]);
        let hostile = build(vec![file("../evil", 1)]);

        // Act
        let ok = scan(Cursor::new(clean), Policy::Copy, &Limits::default(), None);
        let bad = scan(Cursor::new(hostile), Policy::Copy, &Limits::default(), None);

        // Assert
        assert!(ok.is_ok());
        assert!(matches!(bad, Err(ArchiveError::UnsafePath(_))));
    }

    #[test]
    fn given_a_five_gib_entry_header_when_checked_then_the_snapshot_policy_accepts_and_copy_rejects()
     {
        // Arrange
        let five_gib = 5 * 1024 * 1024 * 1024;
        let tar = build(vec![(
            header("big", tar::EntryType::Regular, 0o644, five_gib),
            vec![],
        )]);

        // Act
        let snapshot = scan(
            Cursor::new(&tar),
            Policy::Snapshot,
            &Limits::snapshot(),
            None,
        );
        let copy = scan(Cursor::new(&tar), Policy::Copy, &Limits::default(), None);

        // Assert
        assert!(!matches!(snapshot, Err(ArchiveError::LimitExceeded { .. })));
        assert!(matches!(
            copy,
            Err(ArchiveError::LimitExceeded {
                what: "file size",
                ..
            })
        ));
    }

    #[test]
    fn given_the_snapshot_limits_when_read_then_they_cover_what_linux_allows() {
        // Arrange / Act
        let limits = Limits::snapshot();

        // Assert
        assert_eq!(limits.max_file_bytes, limits.max_total_bytes);
        assert!(limits.max_path_len >= LINUX_PATH_MAX);
        assert!(limits.max_depth >= SNAPSHOT_MAX_DEPTH);
    }

    /// A tar header block of `kind` declaring `size` body bytes.
    fn extended_header(kind: u8, size: u64) -> Vec<u8> {
        let mut h = header("././@LongLink", tar::EntryType::new(kind), 0o644, size);
        h.set_cksum();
        h.as_bytes().to_vec()
    }

    #[rstest]
    #[case::long_name(b'L')]
    #[case::long_link(b'K')]
    #[case::pax(b'x')]
    #[case::pax_global(b'g')]
    fn given_an_oversized_extended_header_when_unpacked_then_it_is_refused_before_its_body_is_read(
        #[case] kind: u8,
    ) {
        // Arrange
        let eight_gib = 8 * 1024 * 1024 * 1024;
        let mut tar = extended_header(kind, eight_gib);
        tar.extend_from_slice(b"tiny");

        // Act
        let started = std::time::Instant::now();
        let (_dest, result) = run(&tar, Policy::Copy, &Limits::default());

        // Assert
        assert!(
            matches!(result, Err(ArchiveError::LimitExceeded { .. })),
            "{result:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn given_a_long_path_under_the_extended_header_cap_when_unpacked_then_it_is_restored() {
        // Arrange
        let name = format!("{}/leaf", vec!["d".repeat(40); 8].join("/"));
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut h, &name, &b"abc"[..]).unwrap();
        let tar = b.into_inner().unwrap();

        // Act
        let (dest, result) = run(&tar, Policy::Copy, &Limits::default());

        // Assert
        result.unwrap();
        assert_eq!(std::fs::read(dest.path().join(&name)).unwrap(), b"abc");
    }

    #[test]
    fn given_a_pax_size_override_when_unpacked_then_it_is_refused() {
        // Arrange
        let record = b"11 size=10\n".to_vec();
        let mut tar = extended_header(b'x', record.len() as u64);
        tar.extend_from_slice(&record);
        tar.resize(tar.len().next_multiple_of(BLOCK_SIZE), 0);

        // Act
        let (_dest, result) = run(&tar, Policy::Snapshot, &Limits::snapshot());

        // Assert
        assert!(matches!(result, Err(ArchiveError::ForbiddenHeader(_))));
    }

    #[test]
    fn given_a_gnu_sparse_entry_when_unpacked_with_the_copy_policy_then_it_is_rejected() {
        // Arrange
        let tar = build(vec![(
            header("holes", tar::EntryType::GNUSparse, 0o644, 0),
            vec![],
        )]);

        // Act
        let (_dest, result) = run(&tar, Policy::Copy, &Limits::default());

        // Assert
        assert!(matches!(result, Err(ArchiveError::ForbiddenType(_))));
    }

    #[cfg(unix)]
    #[test]
    fn given_a_sticky_directory_when_restored_with_the_snapshot_policy_then_the_sticky_bit_stays() {
        // Arrange
        use std::os::unix::fs::PermissionsExt;
        let packed = build(vec![
            (header("tmp", tar::EntryType::Directory, 0o1777, 0), vec![]),
            file("tmp/f", 1),
        ]);

        // Act
        let (dest, result) = run(&packed, Policy::Snapshot, &Limits::snapshot());

        // Assert
        result.unwrap();
        let mode = std::fs::metadata(dest.path().join("tmp"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o1777);
    }

    #[cfg(unix)]
    #[test]
    fn given_a_parent_repointed_at_a_host_dir_when_modes_are_applied_then_the_host_dir_is_untouched()
     {
        // Arrange
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let sentinel = base.path().join("outside/shadow");
        std::fs::create_dir_all(&sentinel).unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = base.path().join("outside");
        let dir = |name: &str, mode| header(name, tar::EntryType::Directory, mode, 0);
        let sym = |target: &str| link_header("a", tar::EntryType::Symlink, target);
        let tar = build(vec![
            (dir("x", 0o755), vec![]),
            (sym("x"), vec![]),
            (dir("a/shadow", 0o777), vec![]),
            (sym(outside.to_str().unwrap()), vec![]),
        ]);

        // Act
        let result = unpack(
            Cursor::new(tar),
            &root,
            Policy::Snapshot,
            &Limits::snapshot(),
        );

        // Assert
        assert!(result.is_err(), "re-pointed parent was accepted");
        let mode = std::fs::metadata(&sentinel).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o700);
    }
}
