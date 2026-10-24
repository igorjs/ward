// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! File transfer: unpack a tar stream into a guest directory and pack a
//! guest path into a tar stream, using the copy policy of `ward-archive`.

use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use ward_archive::{Limits, Policy};

use super::proto::{ArchiveChunk, ArchiveDone, ArchiveError, Event, event, request};
use super::{decode_request, encode_event, read_frame, write_frame};

/// Tar bytes carried by one `ArchiveChunk`, well under the frame cap.
const CHUNK_BYTES: usize = 256 * 1024;

/// Chunks buffered between the socket and the blocking tar worker.
const CHUNK_QUEUE_DEPTH: usize = 8;

/// Where transfers apply and how large they may be.
pub(crate) struct Scope {
    root: PathBuf,
    limits: Limits,
}

impl Scope {
    /// The whole guest filesystem with the default copy caps.
    pub(crate) fn guest() -> Self {
        Self {
            root: PathBuf::from("/"),
            limits: Limits::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(root: PathBuf, limits: Limits) -> Self {
        Self { root, limits }
    }

    /// Map an absolute guest path to a host path under the root.
    fn locate(&self, guest: &str) -> io::Result<PathBuf> {
        let path = Path::new(guest);
        let escapes = path.components().any(|c| matches!(c, Component::ParentDir));
        if !path.is_absolute() || escapes {
            return Err(invalid(format!(
                "guest path must be absolute without `..`: {guest}"
            )));
        }
        let relative = path.strip_prefix("/").unwrap_or(path);
        Ok(self.root.join(relative))
    }

    /// Resolve symlinks and require the result to stay under the root.
    fn contained(&self, located: &Path) -> io::Result<PathBuf> {
        let resolved = located.canonicalize()?;
        if resolved.starts_with(self.root.canonicalize()?) {
            Ok(resolved)
        } else {
            Err(invalid("guest path resolves outside the guest root".into()))
        }
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, kind: event::Kind) -> io::Result<()> {
    write_frame(w, &encode_event(&Event { kind: Some(kind) })).await
}

/// Send the closing frame for a finished transfer.
async fn finish<W: AsyncWrite + Unpin>(w: &mut W, outcome: io::Result<()>) -> io::Result<()> {
    let kind = match outcome {
        Ok(()) => event::Kind::ArchiveDone(ArchiveDone {}),
        Err(e) => event::Kind::ArchiveError(ArchiveError {
            message: e.to_string(),
        }),
    };
    send(w, kind).await
}

/// Unpack the chunks that follow the `PutArchive` frame into `path`.
pub(crate) async fn put<R, W>(
    scope: &Scope,
    path: &str,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let dest = match destination(scope, path) {
        Ok(dest) => dest,
        Err(e) => {
            finish(writer, Err(e)).await?;
            // Read the rest of the upload so the peer sees the error, not a reset.
            drain(reader).await;
            return Ok(());
        }
    };
    let outcome = receive(scope, dest, reader).await;
    finish(writer, outcome).await
}

fn destination(scope: &Scope, path: &str) -> io::Result<PathBuf> {
    let dest = scope.contained(&scope.locate(path)?)?;
    if dest.is_dir() {
        Ok(dest)
    } else {
        Err(invalid(format!("guest path is not a directory: {path}")))
    }
}

/// Discard frames up to the end frame or until the peer hangs up.
async fn drain<R: AsyncRead + Unpin>(reader: &mut R) {
    while let Ok(Some(frame)) = read_frame(reader).await {
        if matches!(
            decode_request(&frame).map(|r| r.kind),
            Ok(Some(request::Kind::ArchiveEnd(_)))
        ) {
            return;
        }
    }
}

async fn receive<R: AsyncRead + Unpin>(
    scope: &Scope,
    dest: PathBuf,
    reader: &mut R,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel(CHUNK_QUEUE_DEPTH);
    let limits = scope.limits;
    let unpacker = tokio::task::spawn_blocking(move || {
        ward_archive::unpack(ChunkReader::new(rx), &dest, Policy::Copy, &limits)
    });
    let fed = feed(reader, tx).await;
    // An unpack failure explains more than a feed failure, so it wins.
    unpacker
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    fed
}

/// Forward chunk frames to the unpacker until the end frame.
async fn feed<R: AsyncRead + Unpin>(reader: &mut R, tx: mpsc::Sender<Vec<u8>>) -> io::Result<()> {
    loop {
        let Some(frame) = read_frame(reader).await? else {
            return Err(invalid("archive upload ended without an end frame".into()));
        };
        match decode_request(&frame)?.kind {
            // A closed queue means the unpacker already finished or failed.
            Some(request::Kind::ArchiveChunk(c)) => {
                if tx.send(c.data).await.is_err() {
                    return Ok(());
                }
            }
            Some(request::Kind::ArchiveEnd(_)) => return Ok(()),
            _ => return Err(invalid("unexpected frame during archive upload".into())),
        }
    }
}

/// Stream a tar of `path` back to the peer.
pub(crate) async fn get<W: AsyncWrite + Unpin>(
    scope: &Scope,
    path: &str,
    writer: &mut W,
) -> io::Result<()> {
    let outcome = transmit(scope, path, writer).await;
    finish(writer, outcome).await
}

async fn transmit<W: AsyncWrite + Unpin>(
    scope: &Scope,
    path: &str,
    writer: &mut W,
) -> io::Result<()> {
    let located = scope.locate(path)?;
    if located.symlink_metadata()?.is_symlink() {
        return Err(invalid(format!("guest path is a symlink: {path}")));
    }
    let source = scope.contained(&located)?;
    let (tx, mut rx) = mpsc::channel(CHUNK_QUEUE_DEPTH);
    let packer = tokio::task::spawn_blocking(move || pack(&source, ChunkWriter::new(tx)));
    while let Some(data) = rx.recv().await {
        send(writer, event::Kind::ArchiveChunk(ArchiveChunk { data })).await?;
    }
    packer.await.map_err(io::Error::other)?
}

// `pack_dir` applies fixed entry, byte and depth caps of its own.
fn pack(source: &Path, writer: ChunkWriter) -> io::Result<()> {
    let mut writer = if source.is_dir() {
        ward_archive::pack_dir(source, writer).map_err(io::Error::other)?
    } else if source.is_file() {
        let name = source
            .file_name()
            .ok_or_else(|| invalid("guest path has no file name".into()))?;
        ward_archive::pack_file(source, Path::new(name), writer).map_err(io::Error::other)?
    } else {
        return Err(invalid("guest path is not a file or directory".into()));
    };
    writer.flush()
}

/// Presents chunks from the socket side as a blocking `Read`.
struct ChunkReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl ChunkReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            current: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.blocking_recv() {
                Some(next) => (self.current, self.pos) = (next, 0),
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.current.len() - self.pos);
        out[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Batches tar bytes into chunks for the socket side.
struct ChunkWriter {
    tx: mpsc::Sender<Vec<u8>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            buf: Vec::new(),
        }
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        for part in data.chunks(CHUNK_BYTES) {
            self.buf.extend_from_slice(part);
            if self.buf.len() >= CHUNK_BYTES {
                self.flush()?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.buf);
        self.tx
            .blocking_send(chunk)
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ArchiveEnd, GetArchive, PutArchive, Request};
    use crate::{handle_connection_scoped, read_frame, write_frame};
    use prost::Message;
    use rstest::rstest;
    use tokio::io::AsyncWriteExt;

    /// Chunk frames in the large body of the drain test.
    const DRAIN_BODY_CHUNKS: usize = 200;
    /// Duplex buffer far smaller than the drain body.
    const SMALL_PIPE: usize = 4096;
    /// Total byte cap used to prove the size limit is wired in.
    const BYTE_CAP: u64 = 1000;
    /// Payload size over `BYTE_CAP`.
    const OVER_BYTE_CAP: usize = 2000;

    /// Small entry cap used to prove the limits are wired in.
    const CAP: usize = 3;
    /// Bytes per chunk frame, small so a tar spans several frames.
    const TEST_CHUNK: usize = 700;

    fn frame(kind: request::Kind) -> Vec<u8> {
        Request { kind: Some(kind) }.encode_to_vec()
    }

    /// Run one connection against `scope`: send `frames`, return the events.
    async fn exchange(scope: &Scope, frames: Vec<Vec<u8>>) -> Vec<Event> {
        let (client, agent) = tokio::io::duplex(1024 * 1024);
        let serve = handle_connection_scoped(agent, scope);
        let talk = async {
            let (mut reader, mut writer) = tokio::io::split(client);
            for f in &frames {
                // The agent may answer and hang up before the last frame.
                let _ = write_frame(&mut writer, f).await;
            }
            let _ = writer.shutdown().await;
            let mut events = Vec::new();
            while let Some(f) = read_frame(&mut reader).await.unwrap() {
                events.push(Event::decode(&f[..]).unwrap());
            }
            events
        };
        let (served, events) = tokio::join!(serve, talk);
        served.unwrap();
        events
    }

    /// Upload `tar` into `path`; `Err` carries the agent's error message.
    async fn put(scope: &Scope, path: &str, tar: &[u8]) -> Result<(), String> {
        let mut frames = vec![frame(request::Kind::PutArchive(PutArchive {
            path: path.into(),
        }))];
        for part in tar.chunks(TEST_CHUNK) {
            frames.push(frame(request::Kind::ArchiveChunk(ArchiveChunk {
                data: part.to_vec(),
            })));
        }
        frames.push(frame(request::Kind::ArchiveEnd(ArchiveEnd {})));
        match exchange(scope, frames).await.pop().and_then(|e| e.kind) {
            Some(event::Kind::ArchiveDone(_)) => Ok(()),
            Some(event::Kind::ArchiveError(e)) => Err(e.message),
            other => panic!("no closing frame: {other:?}"),
        }
    }

    /// Download `path`; returns the tar bytes or the agent's error message.
    async fn get(scope: &Scope, path: &str) -> Result<Vec<u8>, String> {
        let first = frame(request::Kind::GetArchive(GetArchive { path: path.into() }));
        let mut tar = Vec::new();
        for ev in exchange(scope, vec![first]).await {
            match ev.kind {
                Some(event::Kind::ArchiveChunk(c)) => tar.extend(c.data),
                Some(event::Kind::ArchiveDone(_)) => return Ok(tar),
                Some(event::Kind::ArchiveError(e)) => return Err(e.message),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        panic!("no closing frame");
    }

    /// A header with raw path bytes, because `set_path` refuses hostile names.
    fn header(name: &str, kind: tar::EntryType, mode: u32, size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_entry_type(kind);
        h.set_mode(mode);
        h.set_size(size);
        h.set_cksum();
        h
    }

    fn file(name: &str, size: usize) -> (tar::Header, Vec<u8>) {
        (
            header(name, tar::EntryType::Regular, 0o644, size as u64),
            vec![b'x'; size],
        )
    }

    fn build(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (h, data) in entries {
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn scope_in(root: &Path, limits: Limits) -> Scope {
        Scope::new(root.to_path_buf(), limits)
    }

    #[tokio::test]
    async fn given_a_tar_over_the_entry_cap_when_put_then_the_copy_limits_reject_it() {
        // Arrange
        let limits = Limits {
            max_entries: CAP as u64,
            ..Limits::default()
        };
        let upload = |count: usize| {
            let tar = build((0..count).map(|i| file(&format!("f{i}"), 0)).collect());
            async move {
                let root = tempfile::tempdir().unwrap();
                put(&scope_in(root.path(), limits), "/", &tar).await
            }
        };

        // Act
        let at = upload(CAP).await;
        let above = upload(CAP + 1).await;

        // Assert
        assert!(at.is_ok(), "at the cap: {at:?}");
        assert!(above.is_err(), "over the cap was accepted");
    }

    #[tokio::test]
    async fn given_a_parent_dir_entry_when_put_then_it_is_rejected_and_nothing_escapes() {
        // Arrange
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let tar = build(vec![file("../evil", 1)]);
        let scope = scope_in(&root, Limits::default());

        // Act
        let result = put(&scope, "/", &tar).await;

        // Assert
        assert!(result.is_err());
        assert!(!base.path().join("evil").exists());
    }

    #[tokio::test]
    async fn given_a_tar_when_put_then_get_returns_the_same_bytes() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        let scope = scope_in(root.path(), Limits::default());
        let tar = build(vec![file("a/b", 2000)]);

        // Act
        put(&scope, "/dir", &tar).await.unwrap();
        let fetched = get(&scope, "/dir").await.unwrap();
        let single = get(&scope, "/dir/a/b").await.unwrap();

        // Assert
        assert_eq!(
            std::fs::read(root.path().join("dir/a/b")).unwrap(),
            vec![b'x'; 2000]
        );
        let names = |tar: &[u8]| -> Vec<String> {
            tar::Archive::new(tar)
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().display().to_string())
                .collect()
        };
        assert!(names(&fetched).iter().any(|n| n.ends_with("a/b")));
        assert_eq!(names(&single), vec!["b"]);
    }

    #[rstest]
    #[case::relative("relative/path")]
    #[case::parent_dir("/dir/../../etc")]
    #[case::missing("/missing")]
    #[case::symlink("/link")]
    #[case::through_symlink("/link/secret")]
    #[tokio::test]
    async fn given_an_unsafe_path_when_get_then_it_fails_cleanly(#[case] path: &str) {
        // Arrange
        let base = tempfile::tempdir().unwrap();
        let outside = base.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"secret").unwrap();
        let root = base.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let scope = scope_in(&root, Limits::default());

        // Act
        let result = get(&scope, path).await;

        // Assert
        assert!(result.is_err(), "{path} was served");
    }

    #[tokio::test]
    async fn given_a_symlink_entry_then_a_write_through_it_when_put_then_nothing_escapes() {
        // Arrange
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let mut link = header("etc", tar::EntryType::Symlink, 0o777, 0);
        link.set_link_name(&outside).unwrap();
        link.set_cksum();
        let tar = build(vec![(link, Vec::new()), file("etc/evil", 1)]);
        let scope = scope_in(&root, Limits::default());

        // Act
        let result = put(&scope, "/", &tar).await;

        // Assert
        assert!(result.is_err(), "write through a symlink was accepted");
        assert!(!outside.join("evil").exists());
    }

    #[tokio::test]
    async fn given_a_tar_over_the_byte_cap_when_put_then_it_is_rejected() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let limits = Limits {
            max_total_bytes: BYTE_CAP,
            ..Limits::default()
        };
        let tar = build(vec![file("big", OVER_BYTE_CAP)]);

        // Act
        let result = put(&scope_in(root.path(), limits), "/", &tar).await;

        // Assert
        assert!(result.is_err(), "over the byte cap was accepted");
    }

    #[rstest]
    #[case::missing("/missing")]
    #[case::regular_file("/plain")]
    #[tokio::test]
    async fn given_a_non_directory_path_when_put_then_it_fails_cleanly(#[case] path: &str) {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plain"), b"x").unwrap();
        let tar = build(vec![file("f", 1)]);

        // Act
        let result = put(&scope_in(root.path(), Limits::default()), path, &tar).await;

        // Assert
        assert!(result.is_err(), "{path} accepted an upload");
    }

    #[tokio::test]
    async fn given_a_bad_path_when_put_streams_a_large_body_then_the_peer_still_sees_the_error() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let scope = scope_in(root.path(), Limits::default());
        let (client, agent) = tokio::io::duplex(SMALL_PIPE);
        let serve = handle_connection_scoped(agent, &scope);
        let talk = async {
            let (mut reader, mut writer) = tokio::io::split(client);
            let start = frame(request::Kind::PutArchive(PutArchive {
                path: "/missing".into(),
            }));
            write_frame(&mut writer, &start).await.unwrap();
            let chunk = frame(request::Kind::ArchiveChunk(ArchiveChunk {
                data: vec![0; TEST_CHUNK],
            }));
            for _ in 0..DRAIN_BODY_CHUNKS {
                write_frame(&mut writer, &chunk).await.unwrap();
            }
            let end = frame(request::Kind::ArchiveEnd(ArchiveEnd {}));
            write_frame(&mut writer, &end).await.unwrap();
            read_frame(&mut reader).await.unwrap().unwrap()
        };

        // Act
        let (served, reply) = tokio::join!(serve, talk);

        // Assert
        served.unwrap();
        assert!(matches!(
            Event::decode(&reply[..]).unwrap().kind,
            Some(event::Kind::ArchiveError(_))
        ));
    }

    #[tokio::test]
    async fn given_an_exec_frame_mid_upload_when_put_then_it_reports_the_unexpected_frame() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let scope = scope_in(root.path(), Limits::default());
        let frames = vec![
            frame(request::Kind::PutArchive(PutArchive { path: "/".into() })),
            frame(request::Kind::Exec(Default::default())),
        ];

        // Act
        let events = exchange(&scope, frames).await;

        // Assert
        let Some(event::Kind::ArchiveError(e)) = events.last().and_then(|e| e.kind.clone()) else {
            panic!("no error frame: {events:?}");
        };
        assert!(e.message.contains("unexpected frame"), "{}", e.message);
    }

    #[tokio::test]
    async fn given_an_unreadable_file_when_get_then_it_ends_with_an_error() {
        use std::os::unix::fs::PermissionsExt;
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let secret = root.path().join("secret");
        std::fs::write(&secret, b"x").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&secret).is_ok() {
            return; // root ignores file modes
        }
        let scope = scope_in(root.path(), Limits::default());

        // Act
        let result = get(&scope, "/").await;

        // Assert
        assert!(result.is_err(), "unreadable file was packed silently");
    }

    #[tokio::test]
    async fn given_a_put_without_an_end_frame_when_the_peer_hangs_up_then_it_fails() {
        // Arrange
        let root = tempfile::tempdir().unwrap();
        let scope = scope_in(root.path(), Limits::default());
        let start = frame(request::Kind::PutArchive(PutArchive { path: "/".into() }));

        // Act
        let events = exchange(&scope, vec![start]).await;

        // Assert
        assert!(matches!(
            events.last().and_then(|e| e.kind.as_ref()),
            Some(event::Kind::ArchiveError(_))
        ));
    }
}
