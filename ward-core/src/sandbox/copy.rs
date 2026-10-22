// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Relay of a guest tar to a client, checked as it passes.

use std::cell::Cell;
use std::io::{self, Read};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use ward_archive::{ArchiveError, Limits, Policy};

use crate::backend::{BackendError, COPY_CHANNEL_CAPACITY};
use crate::protocol::ApiError;

type Chunk = io::Result<Bytes>;

/// Size of a tar block.
const BLOCK_SIZE: usize = 512;
/// Zero padding a tar may carry after its end marker; more is refused.
const TRAILER_LIMIT_BYTES: usize = 16 * BLOCK_SIZE;
/// How long the guest may send nothing before the copy is abandoned.
const IDLE_DEADLINE: Duration = Duration::from_secs(30);
/// What the guest says when a path does not exist (Linux `strerror`).
const MISSING_PATH_TEXT: &str = "No such file or directory";

/// The sandbox and guest path a copy names, for error messages.
pub(super) struct GuestTarget {
    container: String,
    path: String,
}

impl GuestTarget {
    pub(super) fn new(container: &str, path: &str) -> Self {
        Self {
            container: container.into(),
            path: path.into(),
        }
    }

    /// Map a backend failure of a copy into the sandbox. An `Exec` error is
    /// the guest refusing the request; anything else is on our side.
    pub(super) fn backend_failure(&self, e: BackendError) -> ApiError {
        match e {
            BackendError::NotFound(id) => ApiError::SandboxNotFound(id),
            BackendError::Exec(detail) if detail.contains(MISSING_PATH_TEXT) => self.missing(),
            BackendError::Exec(detail) => ApiError::InvalidRequest(detail),
            other => ApiError::Backend(other.to_string()),
        }
    }

    /// Docker's wording, so scripts that know it recognise the error.
    fn missing(&self) -> ApiError {
        ApiError::InvalidRequest(format!(
            "Could not find the file {} in container {}",
            self.path, self.container
        ))
    }

    /// A refused archive is the caller's problem; a failure to read it is not.
    fn relay_failure(&self, e: ArchiveError) -> ApiError {
        match e {
            ArchiveError::Io(io) if io.to_string().contains(MISSING_PATH_TEXT) => self.missing(),
            ArchiveError::Io(io) => ApiError::Backend(format!("copy from sandbox: {io}")),
            refused => ApiError::InvalidRequest(format!("cannot copy from the sandbox: {refused}")),
        }
    }
}

/// Forward the tar in `source` to the returned receiver, checking it against
/// the copy policy on the way.
///
/// Bytes are forwarded only once the checker has passed the entry that holds
/// them, so a refused archive ends the stream with an `Err` and no byte of the
/// offending header, or of anything after it, has left.
pub(super) fn checked_relay(
    source: mpsc::Receiver<Chunk>,
    limits: Limits,
    target: GuestTarget,
) -> mpsc::Receiver<Result<Bytes, ApiError>> {
    relay_within(source, limits, target, IDLE_DEADLINE)
}

fn relay_within(
    source: mpsc::Receiver<Chunk>,
    limits: Limits,
    target: GuestTarget,
    idle: Duration,
) -> mpsc::Receiver<Result<Bytes, ApiError>> {
    let (out, relayed) = mpsc::channel(COPY_CHANNEL_CAPACITY);
    tokio::task::spawn_blocking(move || {
        let passed = Cell::new(0);
        let mut reader = HoldBack::new(source, out.clone(), idle, &passed);
        let outcome =
            ward_archive::validate_with_progress(&mut reader, Policy::Copy, &limits, |offset| {
                passed.set(offset)
            })
            .and_then(|()| reader.finish());
        if let Err(e) = outcome {
            let _ = out.blocking_send(Err(target.relay_failure(e)));
        }
    });
    relayed
}

/// A `Read` over the guest chunks that forwards bytes to the client only up
/// to the offset the checker has passed.
struct HoldBack<'a> {
    source: mpsc::Receiver<Chunk>,
    out: mpsc::Sender<Result<Bytes, ApiError>>,
    idle: Duration,
    /// Stream offset below which the checker has passed every entry.
    passed: &'a Cell<u64>,
    /// Bytes received and not yet forwarded.
    held: BytesMut,
    /// How far the checker has read into `held`.
    cursor: usize,
    /// Stream offset of the first byte of `held`.
    forwarded: u64,
}

impl<'a> HoldBack<'a> {
    fn new(
        source: mpsc::Receiver<Chunk>,
        out: mpsc::Sender<Result<Bytes, ApiError>>,
        idle: Duration,
        passed: &'a Cell<u64>,
    ) -> Self {
        Self {
            source,
            out,
            idle,
            passed,
            held: BytesMut::new(),
            cursor: 0,
            forwarded: 0,
        }
    }

    /// Check what follows the end of the tar, which may hold only a bounded
    /// run of zero padding, then forward everything left.
    fn finish(&mut self) -> Result<(), ArchiveError> {
        let mut trailing = 0;
        loop {
            let rest = &self.held[self.cursor..];
            if rest.iter().any(|&b| b != 0) {
                return Err(ArchiveError::ForbiddenHeader(
                    "data follows the end of the archive",
                ));
            }
            trailing += rest.len();
            if trailing > TRAILER_LIMIT_BYTES {
                return Err(ArchiveError::LimitExceeded {
                    what: "trailing bytes",
                    limit: TRAILER_LIMIT_BYTES as u64,
                });
            }
            self.cursor = self.held.len();
            if !self.receive()? {
                return self.forward(self.held.len()).map_err(Into::into);
            }
        }
    }

    fn forward_passed(&mut self) -> io::Result<()> {
        let read = self.forwarded + self.cursor as u64;
        let upto = self.passed.get().min(read);
        self.forward(usize::try_from(upto.saturating_sub(self.forwarded)).unwrap_or(0))
    }

    fn forward(&mut self, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let chunk = self.held.split_to(len).freeze();
        self.cursor = self.cursor.saturating_sub(len);
        self.forwarded += len as u64;
        self.out
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client went away"))
    }

    /// Wait for the next guest chunk. False at the end of the stream. A
    /// silent guest or a client that left ends the wait with an error.
    fn receive(&mut self) -> io::Result<bool> {
        let (source, out) = (&mut self.source, &self.out);
        let wait = async {
            tokio::select! {
                chunk = source.recv() => Ok(chunk),
                () = out.closed() => {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "client went away"))
                }
            }
        };
        let chunk = Handle::current()
            .block_on(tokio::time::timeout(self.idle, wait))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("guest sent nothing for {:?}", self.idle),
                )
            })??;
        match chunk {
            None => Ok(false),
            Some(chunk) => {
                self.held.extend_from_slice(&chunk?);
                Ok(true)
            }
        }
    }
}

impl Read for HoldBack<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cursor == self.held.len() {
            self.forward_passed()?;
            if !self.receive()? {
                break;
            }
        }
        let n = buf.len().min(self.held.len() - self.cursor);
        buf[..n].copy_from_slice(&self.held[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use prost::Message;
    use rstest::rstest;
    use tokio::io::{AsyncRead, AsyncWrite};

    use super::*;
    use crate::backend::agent::{self, proto, read_frame, write_frame};
    use proto::{Event, Request, event, request};

    /// Bytes of a tar block, the unit the fake guest slices its tar into so
    /// the relay sees entry headers arrive chunk by chunk.
    const SMALL_CHUNK: usize = 512;
    /// Body of the entry a hostile tar hides; it must never reach the client.
    const HOSTILE_MARKER: &[u8] = b"HOSTILE-BODY";
    /// A file larger than one frame, so a copy spans several chunks.
    const LARGE_FILE_BYTES: usize = 3 * 1024 * 1024;
    const GUEST_DIR: &str = "/data";
    /// Header and body of the first clean test entry.
    const CLEAN_ENTRY_BYTES: usize = BLOCK_SIZE + 4;
    /// A deadline short enough for a test that waits it out.
    const SHORT_IDLE: Duration = Duration::from_millis(50);
    /// Longest a test waits for a background thread to wind down.
    const WIND_DOWN: Duration = Duration::from_secs(5);
    /// A name past the 100 bytes a plain header holds.
    const LONG_NAME_BYTES: usize = 150;

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

    fn link(name: &str, kind: tar::EntryType, target: &str) -> (tar::Header, Vec<u8>) {
        let mut h = header(name, kind, 0o777, 0);
        let field = &mut h.as_gnu_mut().unwrap().linkname;
        field[..target.len()].copy_from_slice(target.as_bytes());
        h.set_cksum();
        (h, vec![])
    }

    fn file(name: &str, mode: u32, body: &[u8]) -> (tar::Header, Vec<u8>) {
        let h = header(name, tar::EntryType::Regular, mode, body.len() as u64);
        (h, body.to_vec())
    }

    fn build(entries: Vec<(tar::Header, Vec<u8>)>) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (h, data) in entries {
            b.append(&h, data.as_slice()).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// A clean entry, then `bad`, so a rejection comes midway through.
    fn after_clean(bad: (tar::Header, Vec<u8>)) -> Vec<u8> {
        build(vec![file("ok", 0o644, b"fine"), bad])
    }

    fn frame(kind: event::Kind) -> Vec<u8> {
        agent::encode(&Event { kind: Some(kind) })
    }

    /// Serve `tar` as a guest agent would answer a `GetArchive`.
    async fn serve_tar<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        tar: Vec<u8>,
        chunk: usize,
    ) {
        let open = read_frame(&mut stream).await.unwrap().unwrap();
        let request = Request::decode(&open[..]).unwrap();
        assert!(matches!(request.kind, Some(request::Kind::GetArchive(_))));
        for piece in tar.chunks(chunk) {
            let chunk = event::Kind::ArchiveChunk(proto::ArchiveChunk {
                data: piece.to_vec(),
            });
            write_frame(&mut stream, &frame(chunk)).await.unwrap();
        }
        let done = event::Kind::ArchiveDone(proto::ArchiveDone {});
        write_frame(&mut stream, &frame(done)).await.unwrap();
    }

    fn target() -> GuestTarget {
        GuestTarget::new("c1", GUEST_DIR)
    }

    /// Collect a relay's output: what it forwarded and how it failed.
    async fn drain(
        mut relayed: mpsc::Receiver<Result<Bytes, ApiError>>,
    ) -> (Vec<u8>, Option<ApiError>) {
        let mut forwarded = Vec::new();
        let mut failure = None;
        while let Some(item) = relayed.recv().await {
            match item {
                Ok(bytes) => forwarded.extend_from_slice(&bytes),
                Err(e) => failure = Some(e),
            }
        }
        (forwarded, failure)
    }

    /// Run the real client path against a fake guest serving `tar` in
    /// frames of `chunk` bytes.
    async fn copy_chunked(
        tar: Vec<u8>,
        limits: Limits,
        chunk: usize,
    ) -> (Vec<u8>, Option<ApiError>) {
        let (daemon, guest) = tokio::io::duplex(SMALL_CHUNK * 4);
        tokio::spawn(serve_tar(guest, tar, chunk));
        let (tx, source) = mpsc::channel(COPY_CHANNEL_CAPACITY);
        tokio::spawn(agent::get_archive(daemon, GUEST_DIR.into(), tx));
        drain(checked_relay(source, limits, target())).await
    }

    /// A GNU long-name entry: a `L` record holding `name`, then a file.
    fn long_named(name: &str, body: &[u8]) -> Vec<u8> {
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.push(0);
        let record = header(
            "././@LongLink",
            tar::EntryType::GNULongName,
            0o644,
            name_bytes.len() as u64,
        );
        build(vec![(record, name_bytes), file("placeholder", 0o644, body)])
    }

    /// A PAX record that overrides the size of the next entry.
    fn pax_size_override() -> Vec<u8> {
        let record = b"11 size=10\n".to_vec();
        let pax = header("pax", tar::EntryType::XHeader, 0o644, record.len() as u64);
        build(vec![(pax, record), file("f", 0o644, HOSTILE_MARKER)])
    }

    fn trailing_data() -> Vec<u8> {
        let mut tar = build(vec![file("ok", 0o644, b"fine")]);
        tar.extend_from_slice(HOSTILE_MARKER);
        tar
    }

    #[rstest]
    #[case::first_header_bad(build(vec![file("../evil", 0o644, HOSTILE_MARKER)]), false, 0)]
    #[case::bad_after_clean(
        after_clean(file("../evil", 0o644, HOSTILE_MARKER)),
        false,
        CLEAN_ENTRY_BYTES
    )]
    #[case::trailing_data(trailing_data(), false, CLEAN_ENTRY_BYTES)]
    #[case::symlink_then_write(
        build(vec![
            link("etc", tar::EntryType::Symlink, "/outside"),
            file("etc/evil", 0o644, HOSTILE_MARKER),
        ]),
        false,
        BLOCK_SIZE
    )]
    #[case::long_name_clean(long_named(&"n".repeat(LONG_NAME_BYTES), b"ok"), true, 0)]
    #[case::long_name_escape(
        long_named(&format!("../{}", "n".repeat(LONG_NAME_BYTES)), HOSTILE_MARKER),
        false,
        0
    )]
    #[case::pax_size_override(pax_size_override(), false, 0)]
    #[tokio::test]
    async fn given_odd_chunk_sizes_when_relayed_then_a_hostile_header_never_leaves(
        #[case] tar: Vec<u8>,
        #[case] accepted: bool,
        #[case] may_leak: usize,
        #[values(1, 7, 513)] chunk: usize,
    ) {
        // Arrange
        let limits = Limits::default();

        // Act
        let (forwarded, failure) = copy_chunked(tar.clone(), limits, chunk).await;

        // Assert
        if accepted {
            assert!(failure.is_none(), "{failure:?}");
            assert_eq!(forwarded, tar);
        } else {
            assert!(matches!(failure, Some(ApiError::InvalidRequest(_))));
            assert!(
                forwarded.len() <= may_leak,
                "{} bytes leaked",
                forwarded.len()
            );
            assert!(tar.starts_with(&forwarded));
        }
    }

    #[tokio::test]
    async fn given_an_endless_zero_trailer_when_relayed_then_it_is_cut_off_and_not_forwarded() {
        // Arrange
        let tar = build(vec![file("a", 0o644, b"fine")]);
        let (tx, source) = mpsc::channel(COPY_CHANNEL_CAPACITY);
        let flood = tokio::spawn(async move {
            tx.send(Ok(Bytes::from(tar.clone()))).await.unwrap();
            while tx
                .send(Ok(Bytes::from(vec![0u8; BLOCK_SIZE])))
                .await
                .is_ok()
            {}
        });

        // Act
        let (forwarded, failure) = drain(checked_relay(source, Limits::default(), target())).await;

        // Assert
        assert!(
            matches!(failure, Some(ApiError::InvalidRequest(_))),
            "{failure:?}"
        );
        assert!(forwarded.len() <= CLEAN_ENTRY_BYTES, "trailer leaked");
        tokio::time::timeout(WIND_DOWN, flood)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn given_a_stalled_guest_when_the_idle_deadline_passes_then_the_relay_fails() {
        // Arrange
        let (_silent, source) = mpsc::channel(COPY_CHANNEL_CAPACITY);

        // Act
        let relayed = relay_within(source, Limits::default(), target(), SHORT_IDLE);
        let (forwarded, failure) = tokio::time::timeout(WIND_DOWN, drain(relayed))
            .await
            .expect("relay never gave up on a silent guest");

        // Assert
        assert!(forwarded.is_empty());
        assert!(matches!(failure, Some(ApiError::Backend(_))), "{failure:?}");
    }

    #[tokio::test]
    async fn given_a_client_that_left_when_the_guest_is_silent_then_the_relay_stops_waiting() {
        // Arrange
        let (guest, source) = mpsc::channel(COPY_CHANNEL_CAPACITY);
        let relayed = relay_within(source, Limits::default(), target(), WIND_DOWN * 100);

        // Act
        drop(relayed);

        // Assert
        tokio::time::timeout(WIND_DOWN, guest.closed())
            .await
            .expect("relay kept waiting on the guest");
    }

    #[rstest]
    #[case::missing(
        target().relay_failure(ArchiveError::Io(io::Error::other("guest agent: No such file or directory"))),
        "invalid request: Could not find the file /data in container c1"
    )]
    #[case::read_failure(
        target().relay_failure(ArchiveError::Io(io::ErrorKind::BrokenPipe.into())),
        "backend error: copy from sandbox: broken pipe"
    )]
    #[case::refused(
        target().relay_failure(ArchiveError::UnsafePath("x".into())),
        "invalid request: cannot copy from the sandbox: archive entry has an unsafe path: x"
    )]
    #[case::exec_missing(
        target().backend_failure(BackendError::Exec("guest agent: No such file or directory".into())),
        "invalid request: Could not find the file /data in container c1"
    )]
    #[case::exec_refusal(
        target().backend_failure(BackendError::Exec("guest path is not a directory".into())),
        "invalid request: guest path is not a directory"
    )]
    fn given_a_failure_when_mapped_then_only_a_refusal_or_missing_path_is_the_callers(
        #[case] mapped: ApiError,
        #[case] expected: &str,
    ) {
        // Assert
        assert_eq!(mapped.to_string(), expected);
    }

    /// Guest filesystem stand-in: unpacks a `PutArchive` and packs a
    /// `GetArchive` under `root` with the same library the real agent uses.
    async fn serve_guest_fs<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, root: &Path) {
        let open = read_frame(&mut stream).await.unwrap().unwrap();
        match Request::decode(&open[..]).unwrap().kind {
            Some(request::Kind::PutArchive(put)) => {
                let mut tar = Vec::new();
                loop {
                    let next = read_frame(&mut stream).await.unwrap().unwrap();
                    match Request::decode(&next[..]).unwrap().kind {
                        Some(request::Kind::ArchiveChunk(c)) => tar.extend_from_slice(&c.data),
                        Some(request::Kind::ArchiveEnd(_)) => break,
                        other => panic!("unexpected frame {other:?}"),
                    }
                }
                let dest = root.join(put.path.trim_start_matches('/'));
                std::fs::create_dir_all(&dest).unwrap();
                ward_archive::unpack(tar.as_slice(), &dest, Policy::Copy, &Limits::default())
                    .unwrap();
                let done = event::Kind::ArchiveDone(proto::ArchiveDone {});
                write_frame(&mut stream, &frame(done)).await.unwrap();
            }
            Some(request::Kind::GetArchive(get)) => {
                let src = root.join(get.path.trim_start_matches('/'));
                let tar = ward_archive::pack_dir(&src, Vec::new()).unwrap();
                serve_pieces(&mut stream, tar).await;
            }
            other => panic!("unexpected opening frame {other:?}"),
        }
    }

    async fn serve_pieces<S: AsyncWrite + Unpin>(stream: &mut S, tar: Vec<u8>) {
        for piece in tar.chunks(agent::ARCHIVE_CHUNK_BYTES) {
            let chunk = event::Kind::ArchiveChunk(proto::ArchiveChunk {
                data: piece.to_vec(),
            });
            write_frame(stream, &frame(chunk)).await.unwrap();
        }
        let done = event::Kind::ArchiveDone(proto::ArchiveDone {});
        write_frame(stream, &frame(done)).await.unwrap();
    }

    fn write_tree(root: &Path) {
        let all_bytes: Vec<u8> = (0..=255u8).cycle().take(LARGE_FILE_BYTES).collect();
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("big.bin"), all_bytes).unwrap();
        std::fs::write(root.join("sub/deep/note.txt"), b"no trailing newline").unwrap();
        std::fs::write(root.join("empty"), b"").unwrap();
        std::os::unix::fs::symlink("big.bin", root.join("sub/link")).unwrap();
    }

    fn read_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.strip_prefix(root).unwrap().display().to_string();
                let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    found.push((name, target.display().to_string().into_bytes()));
                } else {
                    found.push((name, std::fs::read(&path).unwrap()));
                }
            }
        }
        found.sort();
        found
    }

    #[tokio::test]
    async fn given_a_file_tree_when_copied_in_then_out_then_it_is_byte_exact() {
        // Arrange
        let host_src = tempfile::tempdir().unwrap();
        let guest_root = tempfile::tempdir().unwrap();
        let host_out = tempfile::tempdir().unwrap();
        write_tree(host_src.path());
        let tar = ward_archive::pack_dir(host_src.path(), Vec::new()).unwrap();

        // Act: copy in
        let (daemon, guest) = tokio::io::duplex(SMALL_CHUNK * 4);
        let fs_root = guest_root.path().to_path_buf();
        let guest_in = tokio::spawn(async move { serve_guest_fs(guest, &fs_root).await });
        let (feed, chunks) = mpsc::channel(COPY_CHANNEL_CAPACITY);
        let put = tokio::spawn(agent::put_archive(daemon, GUEST_DIR.into(), chunks));
        for piece in tar.chunks(agent::ARCHIVE_CHUNK_BYTES) {
            feed.send(Bytes::copy_from_slice(piece)).await.unwrap();
        }
        drop(feed);
        put.await.unwrap().expect("copy in");
        guest_in.await.unwrap();

        // Act: copy out
        let (daemon, guest) = tokio::io::duplex(SMALL_CHUNK * 4);
        let fs_root = guest_root.path().to_path_buf();
        tokio::spawn(async move { serve_guest_fs(guest, &fs_root).await });
        let (tx, source) = mpsc::channel(COPY_CHANNEL_CAPACITY);
        tokio::spawn(agent::get_archive(daemon, GUEST_DIR.into(), tx));
        let mut relayed = checked_relay(source, Limits::default(), target());
        let mut returned = Vec::new();
        while let Some(item) = relayed.recv().await {
            returned.extend_from_slice(&item.expect("clean tar"));
        }
        ward_archive::unpack(
            returned.as_slice(),
            host_out.path(),
            Policy::Copy,
            &Limits::default(),
        )
        .unwrap();

        // Assert
        assert_eq!(read_tree(host_out.path()), read_tree(host_src.path()));
    }
}
