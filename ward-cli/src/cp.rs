// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward cp`: copy files between the host and a sandbox as tar streams,
//! with `docker cp` rules for where the copy lands.
//!
//! The daemon unpacks an upload into an existing guest directory and serves
//! a download as a tar rooted at the file's own name or, for a directory,
//! at `.`. The CLI turns Docker's placement rules into those two shapes.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;
use ward_core::archive::{self, ArchiveError, Limits, Policy};
use ward_core::pb::copy_to_message::Kind;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{CopyFromChunk, CopyFromRequest, CopyToMessage, CopyToStart};

use crate::client::{self, Resolved};

/// Stands for stdin as a source and stdout as a destination.
const STDIO: &str = "-";
/// Largest tar slice in one `CopyTo` message.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Chunks buffered between the tar thread and the gRPC stream.
const QUEUED_CHUNKS: usize = 4;
const TAR_BLOCK_BYTES: usize = 512;
/// Docker's "copy the contents of" suffix.
const CONTENTS_SUFFIX: &str = "/.";
const GUEST_ROOT: &str = "/";
/// Mode of a symlink entry; the host ignores it.
const SYMLINK_MODE: u32 = 0o777;
/// How the daemon starts the error for a path that is not in the sandbox.
const MISSING_PATH_PREFIX: &str = "Could not find the file";
/// Prefix of the scratch directory a downloaded file is unpacked into.
const STAGING_PREFIX: &str = ".ward-cp-";

/// The non-sandbox side of a copy.
#[derive(Debug, PartialEq)]
enum HostEnd {
    Stdio,
    Path(String),
}

#[derive(Debug, PartialEq)]
enum Transfer {
    Upload {
        source: HostEnd,
        reference: String,
        dest: String,
    },
    Download {
        reference: String,
        source: String,
        dest: HostEnd,
    },
}

enum End {
    Host(HostEnd),
    Sandbox { reference: String, path: String },
}

/// Docker's rule: `NAME:PATH` names a sandbox unless the part before the
/// colon holds a slash, so `./a:b` is a host path.
fn classify(arg: &str) -> End {
    if arg == STDIO {
        return End::Host(HostEnd::Stdio);
    }
    match arg.split_once(':') {
        Some((reference, path)) if !reference.is_empty() && !reference.contains('/') => {
            End::Sandbox {
                reference: reference.to_string(),
                path: path.to_string(),
            }
        }
        _ => End::Host(HostEnd::Path(arg.to_string())),
    }
}

fn parse(src: &str, dest: &str) -> Result<Transfer> {
    match (classify(src), classify(dest)) {
        (End::Host(source), End::Sandbox { reference, path }) => Ok(Transfer::Upload {
            source,
            reference,
            dest: path,
        }),
        (End::Sandbox { reference, path }, End::Host(dest)) => Ok(Transfer::Download {
            reference,
            source: path,
            dest,
        }),
        (End::Sandbox { .. }, End::Sandbox { .. }) => {
            bail!("copying between containers is not supported")
        }
        _ => bail!("must specify at least one container source"),
    }
}

/// A path without Docker's `/.` suffix, and whether the suffix was there.
fn split_contents(raw: &str) -> (String, bool) {
    match raw.strip_suffix(CONTENTS_SUFFIX) {
        Some("") => (GUEST_ROOT.to_string(), true),
        Some(path) => (path.to_string(), true),
        None => (raw.to_string(), false),
    }
}

/// What a path holds, as far as placement cares.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Existing {
    Missing,
    File,
    Dir,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    File,
    /// A host symlink, copied as the link itself.
    Link,
    Dir,
    /// The entries of a directory, not the directory itself.
    Contents,
}

/// Where an upload is unpacked and what its top entry is renamed to.
#[derive(Debug, PartialEq)]
struct Placement {
    dir: String,
    name: Option<String>,
}

fn guest_placement(shape: Shape, base: &str, dest: &str, existing: Existing) -> Result<Placement> {
    let inside = |name: Option<&str>| Placement {
        dir: trim_slash(dest).to_string(),
        name: name.map(str::to_string),
    };
    match (shape, existing) {
        (Shape::Contents, Existing::Dir) => Ok(inside(None)),
        (Shape::Contents, _) => bail!("no such directory in the sandbox: {dest}"),
        (Shape::Dir, Existing::File) => bail!("cannot copy a directory to a file"),
        (_, Existing::Dir) => Ok(inside(Some(base))),
        (Shape::File | Shape::Link, _) if dest.ends_with('/') => {
            bail!("no such directory in the sandbox: {dest}")
        }
        _ => {
            let path = Path::new(dest);
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                bail!("invalid sandbox destination: {dest}");
            };
            Ok(Placement {
                dir: parent.to_string_lossy().into_owned(),
                name: Some(name.to_string_lossy().into_owned()),
            })
        }
    }
}

struct HostSource {
    path: PathBuf,
    shape: Shape,
    base: String,
}

/// With `follow`, a symlink stands for what it points to but keeps its own
/// name in the copy.
fn host_source(raw: &str, follow: bool) -> Result<HostSource> {
    let (path, contents) = if raw == "." {
        (raw.to_string(), true)
    } else {
        split_contents(raw)
    };
    let meta = if follow {
        std::fs::metadata(&path)
    } else {
        std::fs::symlink_metadata(&path)
    }
    .with_context(|| format!("cannot stat {path}"))?;
    if contents && !meta.is_dir() {
        bail!("not a directory: {path}");
    }
    let shape = match (contents, meta.is_dir(), meta.is_symlink()) {
        (true, _, _) => Shape::Contents,
        (false, true, _) => Shape::Dir,
        (false, false, true) => Shape::Link,
        (false, false, false) => Shape::File,
    };
    let base = match (shape, Path::new(&path).file_name()) {
        (Shape::Contents, _) => String::new(),
        (_, Some(name)) => name.to_string_lossy().into_owned(),
        (_, None) => bail!("cannot copy {path}: it has no name"),
    };
    let packed = if follow {
        std::fs::canonicalize(&path).with_context(|| format!("cannot resolve {path}"))?
    } else {
        PathBuf::from(&path)
    };
    Ok(HostSource {
        path: packed,
        shape,
        base,
    })
}

/// Runs `ward cp SRC DEST`.
pub async fn run(
    socket: &str,
    token_store: &Path,
    src: &str,
    dest: &str,
    follow_link: bool,
) -> Result<()> {
    let transfer = parse(src, dest)?;
    let mut c = client::connect(socket).await?;
    match transfer {
        Transfer::Upload {
            source,
            reference,
            dest,
        } => {
            let target = client::resolve(&mut c, token_store, &reference).await?;
            upload(&mut c, &target, source, &dest, follow_link).await
        }
        Transfer::Download {
            reference,
            source,
            dest,
        } => {
            let target = client::resolve(&mut c, token_store, &reference).await?;
            download(&mut c, &target, &source, dest).await
        }
    }
}

// ---------------------------------------------------------------------------
// Host to sandbox
// ---------------------------------------------------------------------------

async fn upload(
    c: &mut WardClient<Channel>,
    target: &Resolved,
    source: HostEnd,
    dest: &str,
    follow_link: bool,
) -> Result<()> {
    let (dest, _) = split_contents(&absolute(&target.workdir, dest));
    let HostEnd::Path(raw) = source else {
        return send(c, target, dest, |mut writer| {
            io::copy(&mut io::stdin().lock(), &mut writer)?;
            writer.flush()
        })
        .await;
    };
    let source = host_source(&raw, follow_link)?;
    let existing = probe(c, target, trim_slash(&dest)).await?;
    let placement = guest_placement(source.shape, &source.base, &dest, existing)?;
    send(c, target, placement.dir, move |writer| {
        pack(&source, placement.name.as_deref(), writer)
    })
    .await
}

/// A directory path without its trailing slash; the root stays `/`.
fn trim_slash(path: &str) -> &str {
    match path.trim_end_matches(GUEST_ROOT) {
        "" => GUEST_ROOT,
        trimmed => trimmed,
    }
}

/// A sandbox path made absolute the way Docker does, from the working
/// directory, or from the root when the sandbox has none.
fn absolute(workdir: &str, path: &str) -> String {
    if path.starts_with(GUEST_ROOT) {
        return path.to_string();
    }
    let base = if workdir.starts_with(GUEST_ROOT) {
        workdir.trim_end_matches(GUEST_ROOT)
    } else {
        ""
    };
    format!("{base}{GUEST_ROOT}{path}")
}

/// Asks the daemon what `path` holds in the sandbox by reading the first
/// header of a `CopyFrom` stream.
async fn probe(c: &mut WardClient<Channel>, target: &Resolved, path: &str) -> Result<Existing> {
    let request = target.request(copy_from_request(target, path))?;
    let outcome = async {
        let mut stream = c.copy_from(request).await?.into_inner();
        read_head(&mut stream).await
    }
    .await;
    match outcome {
        Ok(head) if head.is_empty() => Ok(Existing::Missing),
        Ok(head) if first_entry_is_dir(&head) => Ok(Existing::Dir),
        Ok(_) => Ok(Existing::File),
        Err(status) if is_missing_path(&status) => Ok(Existing::Missing),
        Err(status) => Err(status.into()),
    }
}

fn is_missing_path(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::InvalidArgument
        && status.message().starts_with(MISSING_PATH_PREFIX)
}

fn copy_from_request(target: &Resolved, path: &str) -> CopyFromRequest {
    CopyFromRequest {
        sandbox_id: target.id.clone(),
        path: path.to_string(),
    }
}

/// Streams what `produce` writes to the daemon as one `CopyTo` call.
async fn send(
    c: &mut WardClient<Channel>,
    target: &Resolved,
    dir: String,
    produce: impl FnOnce(ChunkWriter) -> io::Result<()> + Send + 'static,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(QUEUED_CHUNKS);
    let start = CopyToStart {
        sandbox_id: target.id.clone(),
        path: dir,
    };
    let first = CopyToMessage {
        kind: Some(Kind::Start(start)),
    };
    tx.send(first).await.context("the copy stream closed")?;
    let producer = tokio::task::spawn_blocking(move || produce(ChunkWriter::new(tx)));
    let request = target.request(ReceiverStream::new(rx))?;
    if let Err(status) = c.copy_to(request).await {
        // A local failure that cut the tar short explains the daemon's error better.
        if producer.is_finished()
            && let Ok(Err(local)) = producer.await
            && local.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(local.into());
        }
        return Err(status.into());
    }
    producer.await.context("the copy task failed")??;
    Ok(())
}

/// Collects writes into `CopyTo` data messages of at most `CHUNK_BYTES`.
struct ChunkWriter {
    tx: mpsc::Sender<CopyToMessage>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<CopyToMessage>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(CHUNK_BYTES),
        }
    }

    fn send_buffered(&mut self) -> io::Result<()> {
        let data = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK_BYTES));
        let message = CopyToMessage {
            kind: Some(Kind::Data(data)),
        };
        self.tx.blocking_send(message).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the daemon stopped reading the copy",
            )
        })
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let taken = data.len().min(CHUNK_BYTES - self.buf.len());
        self.buf.extend_from_slice(&data[..taken]);
        if self.buf.len() == CHUNK_BYTES {
            self.send_buffered()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            self.send_buffered()
        }
    }
}

fn archive_io(error: ArchiveError) -> io::Error {
    match error {
        ArchiveError::Io(source) => source,
        other => io::Error::other(other),
    }
}

fn pack(source: &HostSource, name: Option<&str>, writer: ChunkWriter) -> io::Result<()> {
    let mut writer = match (source.shape, name) {
        (Shape::Link, name) => pack_symlink(&source.path, name.unwrap_or(&source.base), writer)?,
        (Shape::File, name) => {
            let name = name.unwrap_or(&source.base);
            archive::pack_file(&source.path, Path::new(name), writer).map_err(archive_io)?
        }
        (Shape::Dir, Some(name)) => pack_renamed(&source.path, name, writer)?,
        _ => archive::pack_dir(&source.path, writer).map_err(archive_io)?,
    };
    writer.flush()
}

fn pack_symlink(path: &Path, name: &str, writer: ChunkWriter) -> io::Result<ChunkWriter> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(SYMLINK_MODE);
    let mut builder = tar::Builder::new(writer);
    builder.append_link(&mut header, name, std::fs::read_link(path)?)?;
    builder.into_inner()
}

/// Packs the directory `src` as `name` instead of `.`: the tar is rewritten
/// on the way, because `pack_dir` roots every entry at `.`.
fn pack_renamed(src: &Path, name: &str, writer: ChunkWriter) -> io::Result<ChunkWriter> {
    let (reader, pipe) = io::pipe()?;
    let src = src.to_path_buf();
    let packer = std::thread::spawn(move || archive::pack_dir(&src, pipe).map(drop));
    let renamed = rename_root(reader, name, writer);
    let packed = packer
        .join()
        .map_err(|_| io::Error::other("the tar thread panicked"))?
        .map_err(archive_io);
    match (renamed, packed) {
        (Ok(writer), Ok(())) => Ok(writer),
        // A broken pipe only says the rewrite stopped first.
        (_, Err(error)) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Copies the tar from `reader` to `writer` with the leading `.` of every
/// entry path replaced by `name`.
fn rename_root<R: Read, W: Write>(reader: R, name: &str, writer: W) -> io::Result<W> {
    let mut archive = tar::Archive::new(reader);
    let mut builder = tar::Builder::new(writer);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let rest: PathBuf = entry
            .path()?
            .components()
            .filter_map(|part| match part {
                Component::Normal(part) => Some(part),
                _ => None,
            })
            .collect();
        let path = if rest.as_os_str().is_empty() {
            PathBuf::from(name)
        } else {
            Path::new(name).join(rest)
        };
        let mut header = entry.header().clone();
        match entry.link_name()?.map(|link| link.into_owned()) {
            Some(link) => builder.append_link(&mut header, &path, link)?,
            None => builder.append_data(&mut header, &path, &mut entry)?,
        }
    }
    builder.into_inner()
}

// ---------------------------------------------------------------------------
// Sandbox to host
// ---------------------------------------------------------------------------

/// Where a download is unpacked on the host.
#[derive(Debug, PartialEq)]
enum HostPlan {
    /// Create the directory if needed and unpack the tar into it.
    Into(PathBuf),
    /// Unpack the single file into a scratch directory beside `target`,
    /// then move it to `target`.
    Renamed { parent: PathBuf, target: PathBuf },
}

/// Resolves symlinks in `dest` once, so the plan names the real path.
fn host_plan(dest: &str, guest: &str, contents: bool, is_dir: bool) -> Result<HostPlan> {
    let dest_path = Path::new(dest);
    let (existing, resolved) = match std::fs::canonicalize(dest_path) {
        Ok(path) if path.is_dir() => (Existing::Dir, path),
        Ok(path) => (Existing::File, path),
        Err(_) => (Existing::Missing, missing_dest(dest_path)?),
    };
    match (is_dir, existing) {
        (true, Existing::File) => bail!("cannot copy a directory to a file"),
        (true, Existing::Dir) if !contents => {
            let Some(base) = Path::new(guest).file_name() else {
                bail!("cannot copy {guest} into a directory: it has no name");
            };
            Ok(HostPlan::Into(resolved.join(base)))
        }
        (true, _) | (false, Existing::Dir) => Ok(HostPlan::Into(resolved)),
        (false, _) if dest.ends_with('/') && existing == Existing::Missing => {
            bail!("no such directory: {dest}")
        }
        (false, _) => {
            let parent = resolved.parent().map(Path::to_path_buf).unwrap_or_default();
            Ok(HostPlan::Renamed {
                parent,
                target: resolved,
            })
        }
    }
}

/// A destination that does not exist yet, under its resolved parent, which
/// must exist: nothing above the last component is created.
fn missing_dest(dest: &Path) -> Result<PathBuf> {
    let parent = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let (Ok(parent), Some(name)) = (std::fs::canonicalize(parent), dest.file_name()) else {
        bail!("no such directory: {}", dest.display());
    };
    Ok(parent.join(name))
}

async fn download(
    c: &mut WardClient<Channel>,
    target: &Resolved,
    source: &str,
    dest: HostEnd,
) -> Result<()> {
    let (path, contents) = split_contents(&absolute(&target.workdir, source));
    let request = target.request(copy_from_request(target, &path))?;
    let mut stream = c.copy_from(request).await?.into_inner();
    let head = read_head(&mut stream).await?;
    let is_dir = first_entry_is_dir(&head);
    match dest {
        HostEnd::Stdio => {
            // A directory tar is rooted at `.`; Docker roots it at the name.
            let root = Path::new(&path).file_name().filter(|_| is_dir && !contents);
            let root = root.map(|name| name.to_string_lossy().into_owned());
            relay(stream, head, move |reader| {
                write_stdout(reader, root.as_deref())
            })
            .await
        }
        HostEnd::Path(dest) => {
            let plan = host_plan(&dest, &path, contents, is_dir)?;
            let created = prepare(&plan)?;
            let result = relay(stream, head, move |reader| unpack(reader, &plan)).await;
            if result.is_err()
                && let Some(dir) = created
            {
                let _ = std::fs::remove_dir_all(dir);
            }
            result
        }
    }
}

fn write_stdout(mut reader: StreamReader, root: Option<&str>) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match root {
        Some(name) => drop(rename_root(reader, name, &mut stdout)?),
        None => drop(io::copy(&mut reader, &mut stdout)?),
    }
    stdout.flush()?;
    Ok(())
}

/// Reads until a whole tar header block is in hand, or the stream ends.
async fn read_head(stream: &mut Streaming<CopyFromChunk>) -> Result<Vec<u8>, tonic::Status> {
    let mut head = Vec::new();
    while head.len() < TAR_BLOCK_BYTES {
        match stream.message().await? {
            Some(chunk) => head.extend(chunk.data),
            None => break,
        }
    }
    Ok(head)
}

fn first_entry_is_dir(head: &[u8]) -> bool {
    tar::Archive::new(head)
        .entries()
        .ok()
        .and_then(|mut entries| entries.next())
        .and_then(|entry| entry.ok())
        .is_some_and(|entry| entry.header().entry_type().is_dir())
}

/// Feeds the stream to `consume` on a blocking thread.
async fn relay(
    mut stream: Streaming<CopyFromChunk>,
    head: Vec<u8>,
    consume: impl FnOnce(StreamReader) -> Result<()> + Send + 'static,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(QUEUED_CHUNKS);
    let unpacker = tokio::task::spawn_blocking(move || consume(StreamReader::new(rx, head)));
    let mut failure = None;
    loop {
        match stream.message().await {
            Ok(Some(chunk)) => {
                if tx.send(chunk.data).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(status) => {
                failure = Some(status);
                break;
            }
        }
    }
    drop(tx);
    let unpacked = unpacker.await.context("the unpack task failed")?;
    // The daemon refuses a bad tar before it forwards it, which leaves the
    // unpacker with a cut-off archive; the daemon's reason is the real one.
    match failure {
        Some(status) => Err(status.into()),
        None => unpacked,
    }
}

/// Creates the directory a plan unpacks into, one level only, and returns it
/// when this call made it, so a failed copy can remove it again.
fn prepare(plan: &HostPlan) -> Result<Option<PathBuf>> {
    let HostPlan::Into(dir) = plan else {
        return Ok(None);
    };
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(Some(dir.clone())),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot create {}", dir.display())),
    }
}

/// The tar comes from the guest, so every entry is checked under the copy
/// policy as it is written.
fn unpack(reader: impl Read, plan: &HostPlan) -> Result<()> {
    match plan {
        HostPlan::Into(dir) => unpack_into(reader, dir),
        HostPlan::Renamed { parent, target } => {
            let staging = tempfile::Builder::new()
                .prefix(STAGING_PREFIX)
                .tempdir_in(parent)
                .with_context(|| format!("cannot write in {}", parent.display()))?;
            unpack_into(reader, staging.path())?;
            let entry = std::fs::read_dir(staging.path())?
                .next()
                .context("the sandbox returned an empty archive")??;
            std::fs::rename(entry.path(), target)
                .with_context(|| format!("cannot write {}", target.display()))
        }
    }
}

fn unpack_into(reader: impl Read, dir: &Path) -> Result<()> {
    archive::unpack(reader, dir, Policy::Copy, &Limits::default())?;
    Ok(())
}

/// Presents the chunks of a download as a blocking `Read`.
struct StreamReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current: io::Cursor<Vec<u8>>,
}

impl StreamReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>, head: Vec<u8>) -> Self {
        Self {
            rx,
            current: io::Cursor::new(head),
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let read = self.current.read(buf)?;
            if read > 0 || buf.is_empty() {
                return Ok(read);
            }
            match self.rx.blocking_recv() {
                Some(next) => self.current = io::Cursor::new(next),
                None => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::upload("./f", "web:/tmp", true)]
    #[case::download("web:/tmp/f", "out", true)]
    #[case::path_with_slash_before_colon("./a:b", "web:/tmp", true)]
    #[case::both_sandboxes("a:/x", "b:/y", false)]
    #[case::stdin_to_host("-", "out", false)]
    #[case::stdio_both("-", "-", false)]
    fn given_two_args_when_parsed_then_exactly_one_sandbox_side_is_accepted(
        #[case] src: &str,
        #[case] dest: &str,
        #[case] accepted: bool,
    ) {
        // Arrange / Act
        let result = parse(src, dest);

        // Assert
        assert_eq!(result.is_ok(), accepted, "{src} {dest}");
    }
}
