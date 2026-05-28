// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! OCI image pull, unpack, and local cache management.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use flate2::read::GzDecoder;
use tokio::sync::RwLock;

use super::{BackendError, Result};

// ---------------------------------------------------------------------------
// Cached image entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CachedImage {
    /// OCI image reference (e.g. "python:3.12-slim").
    pub reference: String,
    /// Path to the unpacked rootfs on disk.
    pub rootfs_path: PathBuf,
    /// Digest of the pulled manifest.
    pub digest: String,
    pub pulled_at: std::time::SystemTime,
}

// ---------------------------------------------------------------------------
// ImagePuller
// ---------------------------------------------------------------------------

/// Pulls an image reference and materialises its rootfs into `dest`,
/// returning the manifest digest.
///
/// This is the seam between the cache bookkeeping (`ImageStore`) and the
/// registry protocol (`OciPuller`). Tests inject a fake so cache, list,
/// remove, and traversal behaviour can be exercised offline.
#[async_trait::async_trait]
pub trait ImagePuller: Send + Sync + std::fmt::Debug {
    async fn pull(&self, reference: &str, dest: &Path) -> Result<String>;
}

/// Real puller: talks to an OCI registry over HTTPS, then unpacks each
/// layer tarball into the destination rootfs.
#[derive(Debug, Default)]
pub struct OciPuller;

#[async_trait::async_trait]
impl ImagePuller for OciPuller {
    async fn pull(&self, reference: &str, dest: &Path) -> Result<String> {
        use oci_client::manifest;
        use oci_client::secrets::RegistryAuth;
        use oci_client::{Client, Reference};

        let image_ref: Reference = reference.parse().map_err(|e| {
            BackendError::Image(format!("invalid image reference {reference}: {e}"))
        })?;

        // SEC-019 (part 1): registry allowlist enforcement.
        // WARD_REGISTRY_ALLOWLIST is comma-separated; unset OR
        // empty-after-trim = allow any registry (the documented
        // operator opt-out; the daemon emits a startup warn for the
        // empty-string case so a typo does not silently disable the
        // check). When set with content, image_ref.registry()
        // (defaults to docker.io for unqualified refs) is checked
        // against the list. The lookup tolerates legacy / pasted
        // forms (`https://`, trailing slash, `index.docker.io` ->
        // `docker.io`) so an operator who pastes a URL into the env
        // var does not get a silently-rejecting daemon.
        //
        // Read per-pull deliberately: pulls are network-bound (hundreds
        // of ms minimum) so a 50-ns env-var read is irrelevant, and
        // staying out of Config keeps the OciPuller composable in
        // tests without a startup config dance.
        //
        // Cosign signature verification (the second half of SEC-019) is
        // tracked separately; it requires `sigstore-rs`, a larger
        // dep evaluation.
        if let Ok(raw) = std::env::var("WARD_REGISTRY_ALLOWLIST")
            && !raw.trim().is_empty()
            && !is_registry_allowed(image_ref.registry(), &raw)
        {
            return Err(BackendError::Image(format!(
                "registry {} is not in WARD_REGISTRY_ALLOWLIST ({}); \
                 set WARD_REGISTRY_ALLOWLIST to include {} (e.g. \
                 \"docker.io,ghcr.io,{}\"), or unset the variable to \
                 allow any registry",
                image_ref.registry(),
                raw,
                image_ref.registry(),
                image_ref.registry(),
            )));
        }

        let client = Client::default();
        let auth = RegistryAuth::Anonymous;
        let accepted = vec![
            manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_LAYER_MEDIA_TYPE,
        ];

        let image = client
            .pull(&image_ref, &auth, accepted)
            .await
            .map_err(|e| BackendError::Image(format!("pull {reference} failed: {e}")))?;

        // Layers are applied bottom-up in manifest order; each is a tar
        // diff over the accumulated filesystem.
        for layer in &image.layers {
            unpack_layer(&layer.data, &layer.media_type, dest)?;
        }

        Ok(image.digest.unwrap_or_else(|| "sha256:unknown".to_string()))
    }
}

/// SEC-019: check whether `registry` falls in the comma-separated
/// `WARD_REGISTRY_ALLOWLIST`. Each entry is normalised before compare:
///
/// 1. trim surrounding whitespace
/// 2. strip a leading `http://` or `https://` (operators occasionally
///    paste full URLs)
/// 3. drop trailing `/` (so `docker.io/` and `docker.io` are equivalent)
/// 4. treat `index.docker.io` as an alias for `docker.io` (the legacy
///    Docker hostname; the OCI Reference parser normalises in the
///    opposite direction, so allowlist entries written either way must
///    both match)
///
/// Match is case-insensitive (DNS hostnames are case-insensitive). Empty
/// entries after normalisation are ignored.
fn is_registry_allowed(registry: &str, allowlist: &str) -> bool {
    let needle = normalise_registry(registry);
    allowlist
        .split(',')
        .map(normalise_registry)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry == needle)
}

fn normalise_registry(s: &str) -> String {
    let mut s = s.trim();
    for prefix in ["https://", "http://"] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped;
            break;
        }
    }
    let s = s.trim_end_matches('/').to_ascii_lowercase();
    // Docker Hub legacy hostname `index.docker.io` resolves to the same
    // registry as `docker.io`. The OCI Reference parser emits one or
    // the other depending on the input; allowlist entries written
    // either way must both match.
    if s == "index.docker.io" {
        "docker.io".to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Layer unpacking (pure, registry-independent)
// ---------------------------------------------------------------------------

/// Unpack one image layer tarball into `dest`, applying OCI whiteouts.
///
/// `media_type` selects gzip vs. plain tar. Path traversal is prevented two
/// ways: the `tar` crate refuses to write outside `dest`, and whiteout
/// targets are resolved through [`safe_join`], which rejects `..` and
/// absolute components.
fn unpack_layer(data: &[u8], media_type: &str, dest: &Path) -> Result<()> {
    let reader: Box<dyn Read> = if media_type.ends_with("gzip") {
        Box::new(GzDecoder::new(Cursor::new(data)))
    } else {
        Box::new(Cursor::new(data))
    };

    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    // Never chown to the tar's recorded uid/gid: unpacking runs as an
    // unprivileged user (CI, dev) and ownership is irrelevant until the
    // rootfs is mounted into the guest, where the guest kernel owns it.
    archive.set_preserve_ownerships(false);
    archive.set_overwrite(true);

    let entries = archive
        .entries()
        .map_err(|e| BackendError::Image(format!("read layer tar: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| BackendError::Image(format!("read tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| BackendError::Image(format!("decode tar entry path: {e}")))?
            .into_owned();

        match classify_entry(&path) {
            EntryKind::OpaqueWhiteout(dir) => apply_opaque_whiteout(dest, &dir)?,
            EntryKind::Whiteout(target) => apply_whiteout(dest, &target)?,
            EntryKind::Normal => {
                entry
                    .unpack_in(dest)
                    .map_err(|e| BackendError::Image(format!("unpack tar entry: {e}")))?;
            }
        }
    }
    Ok(())
}

enum EntryKind {
    /// `.wh..wh..opq` — clear all existing contents of the parent dir.
    OpaqueWhiteout(PathBuf),
    /// `.wh.<name>` — delete the sibling `<name>`.
    Whiteout(PathBuf),
    Normal,
}

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

fn classify_entry(path: &Path) -> EntryKind {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return EntryKind::Normal;
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));

    if name == OPAQUE_WHITEOUT {
        EntryKind::OpaqueWhiteout(parent.to_path_buf())
    } else if let Some(removed) = name.strip_prefix(WHITEOUT_PREFIX) {
        EntryKind::Whiteout(parent.join(removed))
    } else {
        EntryKind::Normal
    }
}

/// Delete the rootfs path named by a whiteout marker. Missing targets are
/// not an error: a layer may whiteout something a sibling layer never
/// created, which OCI treats as a no-op.
fn apply_whiteout(dest: &Path, target: &Path) -> Result<()> {
    let Some(full) = safe_join(dest, target) else {
        return Ok(());
    };
    remove_path(&full)
}

/// Clear the contents of a directory named by an opaque whiteout, keeping
/// the directory itself.
fn apply_opaque_whiteout(dest: &Path, dir: &Path) -> Result<()> {
    let Some(full) = safe_join(dest, dir) else {
        return Ok(());
    };
    if !full.is_dir() {
        return Ok(());
    }
    for child in std::fs::read_dir(&full).map_err(BackendError::Io)? {
        let child = child.map_err(BackendError::Io)?;
        remove_path(&child.path())?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(BackendError::Io(e)),
    };
    if meta.is_dir() {
        std::fs::remove_dir_all(path).map_err(BackendError::Io)
    } else {
        std::fs::remove_file(path).map_err(BackendError::Io)
    }
}

/// Join `rel` onto `base`, returning `None` if `rel` is absolute or contains
/// a `..` component that could escape `base`.
fn safe_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            // Absolute roots, drive prefixes, and parent-dir hops are all
            // rejected outright rather than normalised — any of them is a
            // sign of a hostile or malformed layer.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// ImageStore
// ---------------------------------------------------------------------------

/// Manages the local OCI image cache: pull, unpack, and serve rootfs paths.
#[derive(Debug)]
pub struct ImageStore {
    cache_dir: PathBuf,
    images: Arc<RwLock<HashMap<String, CachedImage>>>,
    puller: Arc<dyn ImagePuller>,
}

impl ImageStore {
    /// Production constructor: pulls from real registries.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self::with_puller(cache_dir, Arc::new(OciPuller))
    }

    /// Construct with an injected puller. Tests use this to stay offline.
    pub fn with_puller(cache_dir: PathBuf, puller: Arc<dyn ImagePuller>) -> Self {
        Self {
            cache_dir,
            images: Arc::new(RwLock::new(HashMap::new())),
            puller,
        }
    }

    /// Return the rootfs path for an image, pulling and unpacking if needed.
    pub async fn ensure(&self, reference: &str) -> Result<PathBuf> {
        // Fast path: already cached.
        if let Some(entry) = self.images.read().await.get(reference) {
            return Ok(entry.rootfs_path.clone());
        }

        // Slow path: pull, unpack, cache.
        let (rootfs, digest) = self.pull_and_unpack(reference).await?;

        let entry = CachedImage {
            reference: reference.to_string(),
            rootfs_path: rootfs.clone(),
            digest,
            pulled_at: std::time::SystemTime::now(),
        };
        self.images
            .write()
            .await
            .insert(reference.to_string(), entry);

        Ok(rootfs)
    }

    /// Check whether an image is present in the local cache.
    pub async fn is_cached(&self, reference: &str) -> bool {
        self.images.read().await.contains_key(reference)
    }

    /// Remove an image from the cache, deleting its rootfs directory.
    pub async fn remove(&self, reference: &str) -> Result<()> {
        let entry = self
            .images
            .write()
            .await
            .remove(reference)
            .ok_or_else(|| BackendError::Image(format!("not cached: {}", reference)))?;

        if entry.rootfs_path.exists() {
            std::fs::remove_dir_all(&entry.rootfs_path).map_err(BackendError::Io)?;
        }
        Ok(())
    }

    /// List all cached images.
    pub async fn list(&self) -> Vec<CachedImage> {
        self.images.read().await.values().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn pull_and_unpack(&self, reference: &str) -> Result<(PathBuf, String)> {
        // Use a UUID-based directory name instead of deriving from the image
        // reference. Deriving from user input (e.g. reference.replace('/', "_"))
        // is vulnerable to path traversal if the reference contains ".." or
        // other control sequences that survive the replacement.
        let dir_name = uuid::Uuid::new_v4().to_string();
        let rootfs = self.cache_dir.join(&dir_name).join("rootfs");
        tokio::fs::create_dir_all(&rootfs)
            .await
            .map_err(BackendError::Io)?;

        let digest = self.puller.pull(reference, &rootfs).await?;
        Ok((rootfs, digest))
    }

    /// Validate that a path looks like a usable rootfs.
    fn _validate_rootfs(path: &Path) -> Result<()> {
        if !path.join("bin").exists() && !path.join("usr").exists() {
            return Err(BackendError::Image(format!(
                "rootfs at {} appears incomplete (no /bin or /usr)",
                path.display()
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use tempfile::TempDir;

    // ----- Fake puller: keeps cache/list/remove tests offline -------------

    /// Materialises a minimal rootfs (a `bin/` dir) without touching the
    /// network, so the `ImageStore` bookkeeping can be tested hermetically.
    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl ImagePuller for FakePuller {
        async fn pull(&self, _reference: &str, dest: &Path) -> Result<String> {
            std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
            Ok("sha256:fake".to_string())
        }
    }

    fn store_in_tempdir() -> (ImageStore, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), Arc::new(FakePuller));
        (store, tmp)
    }

    // ----- tar fixture builders -------------------------------------------

    /// Build a gzipped tar layer from (path, contents) file entries.
    fn gzip_layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *contents)
                .expect("append file");
        }
        let tar_bytes = builder.into_inner().expect("finish tar");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).expect("gzip");
        encoder.finish().expect("finish gzip")
    }

    /// Build a gzipped tar layer containing a single explicit-name entry
    /// (used to inject whiteout markers, which aren't real files).
    fn gzip_marker(path: &str) -> Vec<u8> {
        gzip_layer(&[(path, b"")])
    }

    const GZIP_MEDIA: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

    // ----- is_cached: initial state --------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_is_cached_then_returns_false() {
        let (store, _tmp) = store_in_tempdir();
        assert!(!store.is_cached("alpine:latest").await);
    }

    // ----- ensure: happy path --------------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_ensure_then_returns_existing_rootfs_path() {
        let (store, tmp) = store_in_tempdir();
        let rootfs = store.ensure("alpine:latest").await.expect("ensure");
        assert!(rootfs.starts_with(tmp.path()));
        assert!(rootfs.exists());
        assert!(rootfs.ends_with("rootfs"));
    }

    #[tokio::test]
    async fn given_fresh_store_when_ensure_then_marks_image_cached() {
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("ensure");
        assert!(store.is_cached("alpine:latest").await);
    }

    #[tokio::test]
    async fn given_cached_image_when_ensure_again_then_returns_same_path() {
        let (store, _tmp) = store_in_tempdir();
        let first = store.ensure("alpine:latest").await.expect("first ensure");
        let second = store.ensure("alpine:latest").await.expect("second ensure");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn given_two_distinct_references_when_ensure_then_each_gets_unique_path() {
        let (store, _tmp) = store_in_tempdir();
        let alpine = store.ensure("alpine:latest").await.expect("alpine");
        let python = store.ensure("python:3.12-slim").await.expect("python");
        assert_ne!(alpine, python);
        assert!(store.is_cached("alpine:latest").await);
        assert!(store.is_cached("python:3.12-slim").await);
    }

    // ----- remove --------------------------------------------------------

    #[tokio::test]
    async fn given_cached_image_when_remove_then_directory_deleted_and_not_cached() {
        let (store, _tmp) = store_in_tempdir();
        let rootfs = store.ensure("alpine:latest").await.expect("ensure");
        assert!(rootfs.exists());
        store.remove("alpine:latest").await.expect("remove");
        assert!(!store.is_cached("alpine:latest").await);
        assert!(!rootfs.exists());
    }

    #[tokio::test]
    async fn given_uncached_reference_when_remove_then_returns_image_error() {
        let (store, _tmp) = store_in_tempdir();
        let err = store.remove("ghost:latest").await.expect_err("remove");
        match err {
            BackendError::Image(msg) => assert!(msg.contains("ghost:latest"), "got: {msg}"),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    // ----- list ----------------------------------------------------------

    #[tokio::test]
    async fn given_fresh_store_when_list_then_returns_empty() {
        let (store, _tmp) = store_in_tempdir();
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn given_multiple_ensures_when_list_then_returns_all_entries() {
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("alpine");
        store.ensure("python:3.12-slim").await.expect("python");
        let mut refs: Vec<String> = store
            .list()
            .await
            .into_iter()
            .map(|c| c.reference)
            .collect();
        refs.sort();
        assert_eq!(refs, vec!["alpine:latest", "python:3.12-slim"]);
    }

    // ----- security: path traversal regression guard ---------------------

    #[tokio::test]
    async fn given_reference_with_traversal_when_ensure_then_path_stays_under_cache_dir() {
        let (store, tmp) = store_in_tempdir();
        let rootfs = store.ensure("../../../etc/passwd").await.expect("ensure");
        let canonical_root = rootfs.canonicalize().expect("canonical rootfs");
        let canonical_tmp = tmp.path().canonicalize().expect("canonical tmp");
        assert!(
            canonical_root.starts_with(&canonical_tmp),
            "rootfs {} escaped tempdir {}",
            canonical_root.display(),
            canonical_tmp.display(),
        );
    }

    // ----- CachedImage metadata is populated -----------------------------

    #[tokio::test]
    async fn given_ensure_when_list_then_entry_carries_reference_and_digest() {
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("ensure");
        let entry = store
            .list()
            .await
            .into_iter()
            .find(|c| c.reference == "alpine:latest")
            .expect("entry present");
        assert_eq!(entry.reference, "alpine:latest");
        assert_eq!(entry.digest, "sha256:fake");
    }

    // ----- unpack_layer: file extraction ----------------------------------

    #[test]
    fn given_gzip_layer_when_unpack_then_files_written() {
        let tmp = TempDir::new().expect("tempdir");
        let layer = gzip_layer(&[("bin/sh", b"#!/bin/sh\n"), ("etc/hostname", b"ward\n")]);

        unpack_layer(&layer, GZIP_MEDIA, tmp.path()).expect("unpack");

        assert_eq!(
            std::fs::read(tmp.path().join("bin/sh")).expect("read sh"),
            b"#!/bin/sh\n"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("etc/hostname")).expect("read hostname"),
            b"ward\n"
        );
    }

    #[test]
    fn given_plain_tar_layer_when_unpack_then_files_written() {
        let tmp = TempDir::new().expect("tempdir");
        // Build an uncompressed tar and declare a non-gzip media type.
        let mut builder = tar::Builder::new(Vec::new());
        let contents = b"plain";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        builder
            .append_data(&mut header, "file.txt", &contents[..])
            .expect("append");
        let tar_bytes = builder.into_inner().expect("finish");

        unpack_layer(
            &tar_bytes,
            "application/vnd.oci.image.layer.v1.tar",
            tmp.path(),
        )
        .expect("unpack");

        assert_eq!(
            std::fs::read(tmp.path().join("file.txt")).expect("read"),
            b"plain"
        );
    }

    // ----- unpack_layer: whiteouts ----------------------------------------

    #[test]
    fn given_whiteout_marker_when_unpack_then_sibling_removed() {
        let tmp = TempDir::new().expect("tempdir");
        // Lower layer creates a file...
        unpack_layer(
            &gzip_layer(&[("app/data.txt", b"x")]),
            GZIP_MEDIA,
            tmp.path(),
        )
        .expect("lower");
        assert!(tmp.path().join("app/data.txt").exists());

        // ...upper layer whites it out.
        unpack_layer(&gzip_marker("app/.wh.data.txt"), GZIP_MEDIA, tmp.path()).expect("upper");

        assert!(!tmp.path().join("app/data.txt").exists());
        assert!(tmp.path().join("app").is_dir());
    }

    #[test]
    fn given_opaque_whiteout_when_unpack_then_dir_contents_cleared() {
        let tmp = TempDir::new().expect("tempdir");
        unpack_layer(
            &gzip_layer(&[("d/a.txt", b"a"), ("d/b.txt", b"b")]),
            GZIP_MEDIA,
            tmp.path(),
        )
        .expect("lower");

        unpack_layer(&gzip_marker("d/.wh..wh..opq"), GZIP_MEDIA, tmp.path()).expect("opaque");

        assert!(tmp.path().join("d").is_dir());
        assert!(!tmp.path().join("d/a.txt").exists());
        assert!(!tmp.path().join("d/b.txt").exists());
    }

    #[test]
    fn given_whiteout_for_missing_target_when_unpack_then_no_error() {
        let tmp = TempDir::new().expect("tempdir");
        // Whiteout of something that was never created is a no-op, not an error.
        unpack_layer(&gzip_marker("nope/.wh.ghost"), GZIP_MEDIA, tmp.path()).expect("unpack");
    }

    // ----- safe_join: traversal rejection ---------------------------------

    #[test]
    fn given_parent_dir_component_when_safe_join_then_rejected() {
        assert!(safe_join(Path::new("/base"), Path::new("../escape")).is_none());
        assert!(safe_join(Path::new("/base"), Path::new("a/../../escape")).is_none());
    }

    #[test]
    fn given_absolute_path_when_safe_join_then_rejected() {
        assert!(safe_join(Path::new("/base"), Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn given_normal_relative_path_when_safe_join_then_joined() {
        assert_eq!(
            safe_join(Path::new("/base"), Path::new("a/b")),
            Some(PathBuf::from("/base/a/b"))
        );
    }

    // ----- SEC-019: WARD_REGISTRY_ALLOWLIST parsing -----------------------

    #[test]
    fn given_exact_match_when_is_registry_allowed_then_true() {
        assert!(is_registry_allowed("docker.io", "docker.io"));
        assert!(is_registry_allowed("ghcr.io", "docker.io,ghcr.io,quay.io"));
    }

    #[test]
    fn given_no_match_when_is_registry_allowed_then_false() {
        assert!(!is_registry_allowed("evil.io", "docker.io,ghcr.io"));
    }

    #[test]
    fn given_whitespace_around_entries_when_is_registry_allowed_then_trimmed() {
        // Allowlists copy-pasted from docs / env files often contain
        // spaces around commas; tolerate them rather than surprising
        // the operator with "I added it, why is it not working?".
        assert!(is_registry_allowed("ghcr.io", " docker.io ,  ghcr.io  "));
    }

    #[test]
    fn given_empty_entries_when_is_registry_allowed_then_ignored() {
        // Trailing comma, double comma, etc. — common typos. None
        // should match an empty registry string, but the rest of the
        // list should still work.
        assert!(is_registry_allowed("docker.io", "docker.io,,"));
        assert!(!is_registry_allowed("", "docker.io,,ghcr.io"));
    }

    #[test]
    fn given_case_difference_when_is_registry_allowed_then_match() {
        // Registry hostnames are DNS-style and case-insensitive.
        assert!(is_registry_allowed("Docker.IO", "docker.io"));
        assert!(is_registry_allowed("docker.io", "DOCKER.IO"));
    }

    #[test]
    fn given_empty_allowlist_when_is_registry_allowed_then_false() {
        // The function itself rejects an empty list (nothing to match
        // against). The pull-site short-circuits before the call when
        // the env var is empty-after-trim, so this case is the "caller
        // bypassed the short-circuit" contract guard; daemon startup
        // also emits a warn so the empty-string operator footgun is
        // loud, not silent.
        assert!(!is_registry_allowed("docker.io", ""));
        assert!(!is_registry_allowed("docker.io", "   "));
    }

    #[test]
    fn given_index_docker_io_alias_when_is_registry_allowed_then_normalised() {
        // Legacy hostname `index.docker.io` is the same registry as
        // `docker.io`. Allowlists written either way must accept refs
        // produced by oci_client's Reference parser (which may emit
        // either form depending on input).
        assert!(is_registry_allowed("index.docker.io", "docker.io"));
        assert!(is_registry_allowed("docker.io", "index.docker.io"));
    }

    #[test]
    fn given_https_scheme_prefix_when_is_registry_allowed_then_stripped() {
        // Operators occasionally paste full URLs (`https://ghcr.io`)
        // into the env var rather than bare hostnames. Tolerate that
        // rather than silently rejecting every ghcr pull. Same for
        // http://.
        assert!(is_registry_allowed("ghcr.io", "https://ghcr.io"));
        assert!(is_registry_allowed("ghcr.io", "http://ghcr.io"));
    }

    #[test]
    fn given_trailing_slash_when_is_registry_allowed_then_stripped() {
        // `docker.io/` and `docker.io` are equivalent registry
        // identifiers; the trailing-slash form is what you get from
        // copy-paste of a URL like `https://docker.io/`.
        assert!(is_registry_allowed("docker.io", "docker.io/"));
        assert!(is_registry_allowed("docker.io", "https://docker.io/"));
    }

    #[test]
    fn given_localhost_with_port_when_is_registry_allowed_then_exact_match() {
        // Private-registry users pin a port; the normaliser must NOT
        // strip the port. `localhost:5000` only matches itself.
        assert!(is_registry_allowed("localhost:5000", "localhost:5000"));
        assert!(!is_registry_allowed("localhost:5000", "localhost"));
        assert!(!is_registry_allowed("localhost", "localhost:5000"));
    }
}
