// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward cp` against a fake daemon that serves canned tars for `CopyFrom`
//! and records what `CopyTo` receives, so the CLI's own packing and
//! extraction are tested without a guest.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use rstest::rstest;
use tar::{EntryType, Header};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::body::Body;
use tonic::server::NamedService;
use ward_core::pb::copy_to_message::Kind;
use ward_core::pb::{
    CopyFromChunk, CopyFromRequest, CopyToMessage, GetSandboxRequest, SandboxInfo,
};

const SANDBOX_ID: &str = "sb1";
const UNKNOWN_SANDBOX: &str = "ghost";
const WORKDIR: &str = "/srv";
/// A path whose lookup fails the way a daemon outage does.
const FLAKY_PATH: &str = "/flaky";
const SERVED_CHUNK_BYTES: usize = 700;
const VICTIM_BODY: &str = "safe";
const FILE_BODY: &[u8] = b"hello \0 bytes";

/// One finished `CopyTo` call.
struct Upload {
    path: String,
    tar: Vec<u8>,
}

#[derive(Clone, Default)]
struct FakeWard {
    served: Arc<HashMap<String, Vec<u8>>>,
    uploads: Arc<Mutex<Vec<Upload>>>,
}

struct GetSandbox;
struct CopyFrom(FakeWard);
struct CopyTo(FakeWard);

impl tonic::server::UnaryService<GetSandboxRequest> for GetSandbox {
    type Response = SandboxInfo;
    type Future = std::future::Ready<Result<tonic::Response<SandboxInfo>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<GetSandboxRequest>) -> Self::Future {
        let id = request.into_inner().id;
        std::future::ready(if id == UNKNOWN_SANDBOX {
            Err(tonic::Status::not_found(format!("No such container: {id}")))
        } else {
            Ok(tonic::Response::new(SandboxInfo {
                id: SANDBOX_ID.into(),
                workdir: WORKDIR.into(),
                ..Default::default()
            }))
        })
    }
}

impl tonic::server::ServerStreamingService<CopyFromRequest> for CopyFrom {
    type Response = CopyFromChunk;
    type ResponseStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<CopyFromChunk, tonic::Status>>>;
    type Future = std::future::Ready<Result<tonic::Response<Self::ResponseStream>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<CopyFromRequest>) -> Self::Future {
        let path = request.into_inner().path;
        std::future::ready(match self.0.served.get(&path) {
            None if path == FLAKY_PATH => Err(tonic::Status::unavailable("daemon is restarting")),
            None => Err(tonic::Status::invalid_argument(format!(
                "Could not find the file {path} in container {SANDBOX_ID}"
            ))),
            Some(tar) => {
                let chunks: Vec<_> = tar
                    .chunks(SERVED_CHUNK_BYTES)
                    .map(|c| Ok(CopyFromChunk { data: c.to_vec() }))
                    .collect();
                Ok(tonic::Response::new(tokio_stream::iter(chunks)))
            }
        })
    }
}

impl tonic::server::ClientStreamingService<CopyToMessage> for CopyTo {
    type Response = ();
    type Future = Pin<Box<dyn Future<Output = Result<tonic::Response<()>, tonic::Status>> + Send>>;

    fn call(&mut self, request: tonic::Request<tonic::Streaming<CopyToMessage>>) -> Self::Future {
        let uploads = self.0.uploads.clone();
        Box::pin(async move {
            let mut stream = request.into_inner();
            let mut upload = Upload {
                path: String::new(),
                tar: Vec::new(),
            };
            while let Some(message) = stream.message().await? {
                match message.kind {
                    Some(Kind::Start(start)) => upload.path = start.path,
                    Some(Kind::Data(data)) => upload.tar.extend(data),
                    None => {}
                }
            }
            uploads.lock().unwrap().push(upload);
            Ok(tonic::Response::new(()))
        })
    }
}

impl NamedService for FakeWard {
    const NAME: &'static str = "ward.v1.Ward";
}

impl tower::Service<http::Request<Body>> for FakeWard {
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let fake = self.clone();
        Box::pin(async move {
            Ok(match request.uri().path() {
                "/ward.v1.Ward/GetSandbox" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .unary(GetSandbox, request)
                        .await
                }
                "/ward.v1.Ward/CopyFrom" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .server_streaming(CopyFrom(fake), request)
                        .await
                }
                "/ward.v1.Ward/CopyTo" => {
                    tonic::server::Grpc::new(tonic_prost::ProstCodec::default())
                        .client_streaming(CopyTo(fake), request)
                        .await
                }
                _ => tonic::Status::unimplemented("not faked").into_http(),
            })
        })
    }
}

/// A running fake daemon and the scratch directory the CLI runs in.
struct Fixture {
    dir: tempfile::TempDir,
    socket: PathBuf,
    uploads: Arc<Mutex<Vec<Upload>>>,
}

impl Fixture {
    async fn start(served: &[(&str, Vec<u8>)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ward.sock");
        let fake = FakeWard {
            served: Arc::new(
                served
                    .iter()
                    .map(|(path, tar)| (path.to_string(), tar.clone()))
                    .collect(),
            ),
            ..Default::default()
        };
        let uploads = fake.uploads.clone();
        let incoming = UnixListenerStream::new(UnixListener::bind(&socket).unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(fake)
                .serve_with_incoming(incoming),
        );
        // The CLI runs inside `work` so relative host paths stay inside the test's scratch dir.
        std::fs::create_dir(dir.path().join("work")).unwrap();
        Self {
            dir,
            socket,
            uploads,
        }
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    async fn ward(&self, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ward"))
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .current_dir(self.work())
            .env("HOME", self.dir.path())
            .env("XDG_DATA_HOME", self.dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(bytes) = stdin {
            pipe.write_all(bytes).await.unwrap();
        }
        drop(pipe);
        child.wait_with_output().await.unwrap()
    }
}

/// A header whose name is set byte for byte, since `set_path` refuses hostile names.
fn raw_header(name: &str, kind: EntryType, mode: u32, size: usize, link: &str) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_size(size as u64);
    let gnu = header.as_gnu_mut().unwrap();
    gnu.name[..name.len()].copy_from_slice(name.as_bytes());
    gnu.linkname[..link.len()].copy_from_slice(link.as_bytes());
    header.set_cksum();
    header
}

fn tar_of(entries: Vec<(Header, &[u8])>) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (header, body) in entries {
        builder.append(&header, body).unwrap();
    }
    builder.into_inner().unwrap()
}

fn dir_entry(name: &str) -> (Header, &'static [u8]) {
    (raw_header(name, EntryType::Directory, 0o755, 0, ""), b"")
}

fn file_entry<'a>(name: &str, body: &'a [u8]) -> (Header, &'a [u8]) {
    (
        raw_header(name, EntryType::Regular, 0o644, body.len(), ""),
        body,
    )
}

fn entry_names(tar: &[u8]) -> Vec<String> {
    tar::Archive::new(tar)
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn entry_body(tar: &[u8], name: &str) -> Vec<u8> {
    use std::io::Read;
    for entry in tar::Archive::new(tar).entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().ends_with(name) {
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            return body;
        }
    }
    panic!("no entry {name}");
}

fn hostile_tar(kind: &str, outside: &Path) -> Vec<u8> {
    let root = dir_entry(".");
    let escaped = outside.join("escaped");
    match kind {
        "parent_dir" => tar_of(vec![root, file_entry("../../escaped", b"pwned")]),
        "absolute" => tar_of(vec![root, file_entry(escaped.to_str().unwrap(), b"pwned")]),
        "symlink_then_write" => {
            let link = raw_header("link", EntryType::Symlink, 0o777, 0, "../..");
            tar_of(vec![
                root,
                (link, b""),
                file_entry("link/escaped", b"pwned"),
            ])
        }
        "hardlink_escape" => {
            let link = raw_header("h", EntryType::Link, 0o644, 0, "../../victim");
            tar_of(vec![root, (link, b""), file_entry("h", b"pwned")])
        }
        "device" => {
            let node = raw_header("dev", EntryType::Char, 0o666, 0, "");
            tar_of(vec![root, (node, b"")])
        }
        "setuid" => {
            let file = raw_header("suid", EntryType::Regular, 0o4755, 1, "");
            tar_of(vec![root, (file, b"x")])
        }
        other => panic!("unknown case {other}"),
    }
}

#[rstest]
#[case::parent_dir("parent_dir")]
#[case::absolute("absolute")]
#[case::symlink_then_write("symlink_then_write")]
#[case::hardlink_escape("hardlink_escape")]
#[case::device("device")]
#[case::setuid("setuid")]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_hostile_tar_when_cp_from_then_nothing_is_written_outside_out(#[case] kind: &str) {
    // Arrange
    let outside = tempfile::tempdir().unwrap();
    let tar = hostile_tar(kind, outside.path());
    let fixture = Fixture::start(&[("/d", tar)]).await;
    let work = fixture.work();
    std::fs::create_dir(work.join("out")).unwrap();
    std::fs::write(work.join("victim"), VICTIM_BODY).unwrap();

    // Act
    let output = fixture.ward(&["cp", "sb:/d", "out"], None).await;

    // Assert
    assert!(!output.status.success(), "{kind} was accepted");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("archive"),
        "{kind} was not refused by the archive check: {stderr}"
    );
    let mut names: Vec<_> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["out", "victim"], "{kind} wrote outside out");
    assert_eq!(
        std::fs::read_to_string(work.join("victim")).unwrap(),
        VICTIM_BODY
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_tar_on_stdin_when_cp_into_a_sandbox_then_the_same_bytes_arrive() {
    // Arrange
    let tar = tar_of(vec![file_entry("f", FILE_BODY)]);
    let fixture = Fixture::start(&[]).await;

    // Act
    let output = fixture.ward(&["cp", "-", "sb:/dir"], Some(&tar)).await;

    // Assert
    assert!(output.status.success(), "{output:?}");
    let uploads = fixture.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].path, "/dir");
    assert_eq!(uploads[0].tar, tar);
    assert_eq!(entry_body(&uploads[0].tar, "f"), FILE_BODY);
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_sandbox_file_when_cp_to_stdout_then_a_tar_of_it_is_written() {
    // Arrange
    let tar = tar_of(vec![file_entry("f", FILE_BODY)]);
    let fixture = Fixture::start(&[("/f", tar.clone())]).await;

    // Act
    let output = fixture.ward(&["cp", "sb:/f", "-"], None).await;

    // Assert
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, tar);
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_sandbox_file_when_cp_to_a_new_name_then_the_host_file_has_the_bytes() {
    // Arrange
    let fixture = Fixture::start(&[("/f", tar_of(vec![file_entry("f", FILE_BODY)]))]).await;

    // Act
    let output = fixture.ward(&["cp", "sb:/f", "renamed"], None).await;

    // Assert
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(fixture.work().join("renamed")).unwrap(),
        FILE_BODY
    );
    assert_eq!(std::fs::read_dir(fixture.work()).unwrap().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_host_dir_when_cp_to_a_missing_guest_path_then_the_dir_is_renamed_to_it() {
    // Arrange
    let fixture = Fixture::start(&[]).await;
    let src = fixture.work().join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), FILE_BODY).unwrap();

    // Act
    let output = fixture.ward(&["cp", "src", "sb:/tmp/newdir"], None).await;

    // Assert
    assert!(output.status.success(), "{output:?}");
    let uploads = fixture.uploads.lock().unwrap();
    assert_eq!(uploads[0].path, "/tmp");
    assert_eq!(entry_names(&uploads[0].tar), ["newdir", "newdir/a.txt"]);
}

fn exec_entry<'a>(name: &str, body: &'a [u8]) -> (Header, &'a [u8]) {
    let header = raw_header(name, EntryType::Regular, 0o755, body.len(), "");
    (header, body)
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[rstest]
#[case::absolute("sb:/bin/tool")]
#[case::relative_to_the_workdir("sb:bin/tool")]
#[tokio::test(flavor = "multi_thread")]
async fn given_an_executable_sandbox_file_when_cp_then_the_exec_bit_survives(#[case] source: &str) {
    // Arrange
    let tar = tar_of(vec![exec_entry("tool", b"#!")]);
    let fixture = Fixture::start(&[("/bin/tool", tar.clone()), ("/srv/bin/tool", tar)]).await;

    // Act
    let output = fixture.ward(&["cp", source, "./tool"], None).await;

    // Assert
    assert!(output.status.success(), "{}", stderr_of(&output));
    let mode = std::fs::metadata(fixture.work().join("tool"))
        .unwrap()
        .permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o7777,
        0o755
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_sandbox_dir_when_cp_to_stdout_then_the_tar_is_rooted_at_its_name() {
    // Arrange
    let tar = tar_of(vec![dir_entry("."), file_entry("./a.txt", FILE_BODY)]);
    let fixture = Fixture::start(&[("/data", tar)]).await;

    // Act
    let output = fixture.ward(&["cp", "sb:/data", "-"], None).await;

    // Assert
    assert!(output.status.success(), "{output:?}");
    let names: Vec<_> = entry_names(&output.stdout)
        .into_iter()
        .map(|n| n.trim_end_matches('/').to_string())
        .collect();
    assert_eq!(names, ["data", "data/a.txt"]);
}

#[rstest]
#[case::the_link_itself(&["cp", "link", "sb:/dir"], EntryType::Symlink)]
#[case::the_target_when_followed(&["cp", "-L", "link", "sb:/dir"], EntryType::Regular)]
#[case::archive_and_quiet_flags(&["cp", "-a", "-q", "link", "sb:/dir"], EntryType::Symlink)]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_host_symlink_when_cp_then_the_link_or_its_target_is_sent(
    #[case] args: &[&str],
    #[case] kind: EntryType,
) {
    // Arrange
    let fixture = Fixture::start(&[("/dir", tar_of(vec![dir_entry(".")]))]).await;
    std::fs::write(fixture.work().join("target"), FILE_BODY).unwrap();
    std::os::unix::fs::symlink("target", fixture.work().join("link")).unwrap();

    // Act
    let output = fixture.ward(args, None).await;

    // Assert
    assert!(output.status.success(), "{}", stderr_of(&output));
    let uploads = fixture.uploads.lock().unwrap();
    let mut archive = tar::Archive::new(uploads[0].tar.as_slice());
    let entry = archive.entries().unwrap().next().unwrap().unwrap();
    assert_eq!(entry.header().entry_type(), kind);
    assert_eq!(entry.path().unwrap().to_str(), Some("link"));
}

#[rstest]
#[case::file_to_missing_parent(&["cp", "sb:/f", "nope/x"])]
#[case::file_to_missing_dir_with_slash(&["cp", "sb:/f", "nope/"])]
#[case::dir_to_missing_parent(&["cp", "sb:/data", "nope/x"])]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_missing_host_parent_when_cp_then_no_such_directory_and_nothing_is_made(
    #[case] args: &[&str],
) {
    // Arrange
    let served = [
        ("/f", tar_of(vec![file_entry("f", FILE_BODY)])),
        ("/data", tar_of(vec![dir_entry(".")])),
    ];
    let fixture = Fixture::start(&served).await;

    // Act
    let output = fixture.ward(args, None).await;

    // Assert
    assert!(
        stderr_of(&output).contains("no such directory"),
        "{output:?}"
    );
    assert_eq!(std::fs::read_dir(fixture.work()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_failed_extract_into_a_new_dir_when_cp_then_the_new_dir_is_removed() {
    // Arrange
    let tar = hostile_tar("parent_dir", Path::new("/unused"));
    let fixture = Fixture::start(&[("/d", tar)]).await;

    // Act
    let output = fixture.ward(&["cp", "sb:/d", "fresh"], None).await;

    // Assert
    assert!(!output.status.success());
    assert!(!fixture.work().join("fresh").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn given_a_lookup_that_fails_for_another_reason_when_cp_up_then_the_error_is_reported() {
    // Arrange
    let fixture = Fixture::start(&[]).await;
    std::fs::write(fixture.work().join("f.txt"), FILE_BODY).unwrap();

    // Act
    let output = fixture.ward(&["cp", "f.txt", "sb:/flaky"], None).await;

    // Assert
    assert!(
        stderr_of(&output).contains("daemon is restarting"),
        "{output:?}"
    );
    assert!(fixture.uploads.lock().unwrap().is_empty());
}

#[rstest]
#[case::inside_by_name("out", "sb:/data", "out/data/a.txt")]
#[case::contents("out", "sb:/data/.", "out/a.txt")]
#[case::through_a_symlink("link", "sb:/data", "real/data/a.txt")]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_sandbox_dir_when_cp_to_an_existing_dir_then_docker_cp_placement_applies(
    #[case] dest: &str,
    #[case] source: &str,
    #[case] landed: &str,
) {
    // Arrange
    let tar = tar_of(vec![dir_entry("."), file_entry("./a.txt", FILE_BODY)]);
    let fixture = Fixture::start(&[("/data", tar)]).await;
    let work = fixture.work();
    std::fs::create_dir_all(work.join("out")).unwrap();
    std::fs::create_dir(work.join("real")).unwrap();
    std::os::unix::fs::symlink("real", work.join("link")).unwrap();

    // Act
    let output = fixture.ward(&["cp", source, dest], None).await;

    // Assert
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(std::fs::read(work.join(landed)).unwrap(), FILE_BODY);
}

#[rstest]
#[case::file("f.txt", "sb:/dir")]
#[case::file_with_slash("f.txt", "sb:/dir/")]
#[case::contents_to_contents("src/.", "sb:/dir/.")]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_host_source_when_cp_to_an_existing_guest_dir_then_the_entries_go_inside(
    #[case] source: &str,
    #[case] dest: &str,
) {
    // Arrange
    let fixture = Fixture::start(&[("/dir", tar_of(vec![dir_entry(".")]))]).await;
    let work = fixture.work();
    std::fs::write(work.join("f.txt"), FILE_BODY).unwrap();
    std::fs::create_dir(work.join("src")).unwrap();
    std::fs::write(work.join("src/f.txt"), FILE_BODY).unwrap();

    // Act
    let output = fixture.ward(&["cp", source, dest], None).await;

    // Assert
    assert!(output.status.success(), "{}", stderr_of(&output));
    let uploads = fixture.uploads.lock().unwrap();
    let names = entry_names(&uploads[0].tar);
    assert_eq!(uploads[0].path, "/dir");
    assert!(names.iter().any(|n| n.ends_with("f.txt")), "{names:?}");
    assert!(names.iter().all(|n| !n.contains("src")), "{names:?}");
}

#[rstest]
#[case::overwritten("old", "old")]
#[case::written_through_a_symlink("link", "old")]
#[tokio::test(flavor = "multi_thread")]
async fn given_an_existing_host_file_when_cp_down_then_the_real_file_is_replaced(
    #[case] dest: &str,
    #[case] real: &str,
) {
    // Arrange
    let fixture = Fixture::start(&[("/f", tar_of(vec![file_entry("f", FILE_BODY)]))]).await;
    let work = fixture.work();
    std::fs::write(work.join(real), "stale and longer than the new body").unwrap();
    std::os::unix::fs::symlink(real, work.join("link")).unwrap();

    // Act
    let output = fixture.ward(&["cp", "sb:/f", dest], None).await;

    // Assert
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(std::fs::read(work.join(real)).unwrap(), FILE_BODY);
    assert!(work.join("link").is_symlink());
}

#[rstest]
#[case::unknown_sandbox("ghost:/f", "No such container: ghost")]
#[case::missing_path("sb:/nope", "Could not find the file /nope in container sb1")]
#[tokio::test(flavor = "multi_thread")]
async fn given_a_missing_sandbox_or_path_when_cp_then_the_error_is_in_docker_wording(
    #[case] source: &str,
    #[case] reason: &str,
) {
    // Arrange
    let fixture = Fixture::start(&[]).await;

    // Act
    let output = fixture.ward(&["cp", source, "out"], None).await;

    // Assert
    assert_eq!(
        stderr_of(&output).trim(),
        format!("Error response from daemon: {reason}")
    );
}
