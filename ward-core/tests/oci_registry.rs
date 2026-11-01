// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The real `OciPuller` against an in-process registry: manifests, blobs,
//! layer order, tag re-resolution and the error codes a pull maps to.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use ward_core::backend::BackendError;
use ward_core::backend::image::{ImageStore, OciPuller};

const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const REPOSITORY: &str = "team/app";
const MAX_CACHED_IMAGES: usize = 16;
const HEADER_END: &[u8] = b"\r\n\r\n";
const MANIFEST_UNKNOWN_BODY: &str =
    r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown"}]}"#;

struct Route {
    content_type: &'static str,
    body: Vec<u8>,
}

/// A registry that serves what the test publishes and counts blob requests.
struct FakeRegistry {
    addr: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    blob_requests: Arc<Mutex<usize>>,
}

impl FakeRegistry {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let blob_requests: Arc<Mutex<usize>> = Arc::default();
        let (served_routes, served_blobs) = (routes.clone(), blob_requests.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, served_routes.clone(), served_blobs.clone()));
            }
        });
        Self {
            addr,
            routes,
            blob_requests,
        }
    }

    fn reference(&self, tag: &str) -> String {
        format!("{}/{REPOSITORY}:{tag}", self.addr)
    }

    fn store(&self, cache: &Path) -> ImageStore {
        let puller = OciPuller::with_http_registry(self.addr.to_string());
        ImageStore::with_puller(cache.to_path_buf(), MAX_CACHED_IMAGES, Arc::new(puller))
    }

    fn blob_requests(&self) -> usize {
        *self.blob_requests.lock().expect("blob counter")
    }

    fn set(&self, path: String, content_type: &'static str, body: Vec<u8>) {
        self.routes
            .lock()
            .expect("routes")
            .insert(path, Route { content_type, body });
    }

    /// Serve `layers` (bottom first) as `tag` and return the manifest digest.
    fn publish(&self, tag: &str, layers: &[Vec<u8>]) -> String {
        let config = br#"{"config":{"Cmd":["/bin/sh"]}}"#.to_vec();
        let mut descriptors = Vec::new();
        for layer in layers {
            descriptors.push(self.publish_blob(OCI_LAYER_TYPE, layer));
        }
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST_TYPE,
            "config": self.publish_blob(OCI_CONFIG_TYPE, &config),
            "layers": descriptors,
        }))
        .expect("manifest");
        self.set(
            format!("/v2/{REPOSITORY}/manifests/{tag}"),
            OCI_MANIFEST_TYPE,
            manifest.clone(),
        );
        sha256_digest(&manifest)
    }

    fn publish_blob(&self, media_type: &'static str, bytes: &[u8]) -> serde_json::Value {
        let digest = sha256_digest(bytes);
        self.set(
            format!("/v2/{REPOSITORY}/blobs/{digest}"),
            "application/octet-stream",
            bytes.to_vec(),
        );
        serde_json::json!({
            "mediaType": media_type,
            "digest": digest,
            "size": bytes.len(),
        })
    }

    /// Make the blob with `digest` longer than its descriptor declares.
    fn pad_blob(&self, digest: &str, extra: usize) {
        let path = format!("/v2/{REPOSITORY}/blobs/{digest}");
        let mut routes = self.routes.lock().expect("routes");
        routes
            .get_mut(&path)
            .expect("published blob")
            .body
            .extend(vec![0u8; extra]);
    }
}

fn sha256_digest(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    blob_requests: Arc<Mutex<usize>>,
) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    while !request.windows(HEADER_END.len()).any(|w| w == HEADER_END) {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&request).into_owned();
    let mut parts = text.lines().next().unwrap_or_default().split(' ');
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if path.contains("/blobs/") {
        *blob_requests.lock().expect("blob counter") += 1;
    }

    let (status, content_type, body, digest) = if path == "/v2/" {
        ("200 OK", "application/json", b"{}".to_vec(), None)
    } else if let Some(route) = routes.lock().expect("routes").get(path) {
        let digest = path
            .contains("/manifests/")
            .then(|| sha256_digest(&route.body));
        ("200 OK", route.content_type, route.body.clone(), digest)
    } else {
        (
            "404 Not Found",
            "application/json",
            MANIFEST_UNKNOWN_BODY.as_bytes().to_vec(),
            None,
        )
    };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(digest) = digest {
        head.push_str(&format!("Docker-Content-Digest: {digest}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    if method != "HEAD" {
        let _ = stream.write_all(&body).await;
    }
    let _ = stream.shutdown().await;
}

enum Entry<'a> {
    File(&'a str, &'a str),
    Link(&'a str, &'a str),
}

fn layer(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        match entry {
            Entry::File(path, contents) => {
                header.set_size(contents.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, contents.as_bytes())
                    .expect("file");
            }
            Entry::Link(path, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                builder
                    .append_link(&mut header, path, target)
                    .expect("link");
            }
        }
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&builder.into_inner().expect("tar"))
        .expect("gzip");
    encoder.finish().expect("gzip")
}

fn read(root: &Path, relative: &str) -> Option<String> {
    std::fs::read_to_string(root.join(relative)).ok()
}

#[tokio::test]
async fn given_three_layers_when_pulled_then_they_apply_bottom_up_with_whiteouts_through_a_symlink()
{
    // Arrange
    let registry = FakeRegistry::start().await;
    let base = layer(&[
        Entry::File("usr/bin/ls", "ls"),
        Entry::File("usr/bin/rm", "rm"),
        Entry::Link("bin", "usr/bin"),
        Entry::File("etc/a", "one"),
        Entry::File("etc/b", "base"),
    ]);
    let middle = layer(&[
        Entry::File("etc/a", "two"),
        Entry::File("etc/.wh.b", ""),
        Entry::File("bin/.wh.rm", ""),
    ]);
    let top = layer(&[Entry::File("etc/b", "restored")]);
    let manifest_digest = registry.publish("1", &[base, middle, top]);
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());

    // Act
    let rootfs = store.ensure(&registry.reference("1")).await.expect("pull");

    // Assert
    assert_eq!(read(&rootfs, "etc/a").as_deref(), Some("two"));
    assert_eq!(read(&rootfs, "etc/b").as_deref(), Some("restored"));
    assert_eq!(read(&rootfs, "usr/bin/ls").as_deref(), Some("ls"));
    assert_eq!(read(&rootfs, "usr/bin/rm"), None);
    let listed = store.list().await;
    assert_eq!(listed[0].digest, manifest_digest);
}

#[tokio::test]
async fn given_an_unchanged_tag_when_pulled_again_then_it_is_up_to_date_without_downloading() {
    // Arrange
    let registry = FakeRegistry::start().await;
    registry.publish("1", &[layer(&[Entry::File("a", "one")])]);
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());
    let reference = registry.reference("1");
    store.pull(&reference, None).await.expect("first pull");
    let blobs_after_first = registry.blob_requests();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Act
    store.pull(&reference, Some(tx)).await.expect("second pull");

    // Assert
    let mut last = String::new();
    while let Some(line) = rx.recv().await {
        last = line.status;
    }
    assert_eq!(last, format!("Status: Image is up to date for {reference}"));
    assert_eq!(registry.blob_requests(), blobs_after_first);
}

#[tokio::test]
async fn given_a_tag_that_moved_when_pulled_again_then_the_new_image_is_downloaded_and_tagged() {
    // Arrange
    let registry = FakeRegistry::start().await;
    let old_digest = registry.publish("1", &[layer(&[Entry::File("a", "one")])]);
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());
    let reference = registry.reference("1");
    store.pull(&reference, None).await.expect("first pull");
    let new_digest = registry.publish("1", &[layer(&[Entry::File("a", "two")])]);

    // Act
    let pulled = store.pull(&reference, None).await.expect("second pull");

    // Assert
    assert_eq!(pulled.digest, new_digest);
    assert_eq!(read(&pulled.rootfs_path, "a").as_deref(), Some("two"));
    let rows: HashMap<_, _> = store
        .list()
        .await
        .into_iter()
        .map(|image| (image.digest, image.reference))
        .collect();
    assert_eq!(rows[&new_digest], reference);
    assert_eq!(rows[&old_digest], old_digest);
}

#[tokio::test]
async fn given_two_tags_of_one_image_when_pulled_then_both_are_listed_and_cached_once() {
    // Arrange
    let registry = FakeRegistry::start().await;
    let layers = [layer(&[Entry::File("a", "one")])];
    registry.publish("latest", &layers);
    registry.publish("1.0", &layers);
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());

    // Act
    store
        .pull(&registry.reference("latest"), None)
        .await
        .expect("latest");
    store
        .pull(&registry.reference("1.0"), None)
        .await
        .expect("1.0");

    // Assert
    let mut references: Vec<_> = store
        .list()
        .await
        .into_iter()
        .map(|image| image.reference)
        .collect();
    references.sort();
    assert_eq!(
        references,
        vec![registry.reference("1.0"), registry.reference("latest")]
    );
}

#[tokio::test]
async fn given_an_unknown_tag_when_pulled_then_the_manifest_is_unknown() {
    // Arrange
    let registry = FakeRegistry::start().await;
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());
    let reference = registry.reference("missing");

    // Act
    let error = store.pull(&reference, None).await.expect_err("unknown tag");

    // Assert
    assert!(
        matches!(&error, BackendError::ImageNotFound(m)
            if m == &format!("manifest for {reference} not found: manifest unknown")),
        "{error}"
    );
}

#[tokio::test]
async fn given_a_blob_longer_than_declared_when_pulled_then_it_is_resource_exhausted() {
    // Arrange
    let registry = FakeRegistry::start().await;
    let blob = layer(&[Entry::File("a", "one")]);
    registry.publish("1", std::slice::from_ref(&blob));
    registry.pad_blob(&sha256_digest(&blob), 64);
    let cache = TempDir::new().expect("cache");
    let store = registry.store(cache.path());

    // Act
    let error = store
        .pull(&registry.reference("1"), None)
        .await
        .expect_err("oversize blob");

    // Assert
    assert!(
        matches!(error, BackendError::ResourceExhausted(_)),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn given_a_registry_that_is_down_when_pulled_then_it_is_unavailable() {
    // Arrange
    let cache = TempDir::new().expect("cache");
    // A port nothing listens on: the OS refuses the connection.
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let closed_addr = closed.local_addr().expect("addr");
    drop(closed);
    let down = ImageStore::with_puller(
        cache.path().to_path_buf(),
        MAX_CACHED_IMAGES,
        Arc::new(OciPuller::with_http_registry(closed_addr.to_string())),
    );

    // Act
    let error = down
        .pull(&format!("{closed_addr}/{REPOSITORY}:1"), None)
        .await
        .expect_err("registry down");

    // Assert
    assert!(matches!(error, BackendError::Unavailable(_)), "{error}");
}
