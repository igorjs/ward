// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `docker save` and `docker load` for the image cache.
//!
//! A saved archive is an OCI image layout that also carries Docker's
//! `manifest.json`, so both `docker load` and `podman load` read it. The cache
//! keeps unpacked root filesystems, not the registry's layers, so each image
//! is written as one flattened layer. Owners are not kept in the cache, so
//! the layer records root, and directory and symlink times are zero. This
//! makes the bytes depend only on the cached files: saving an image that was
//! loaded gives the archive it came from, and the same manifest digest.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::manage::Matched;
use super::{
    BackendError, CacheMeta, DIGEST_ALGORITHM_PREFIX, HistoryEntry, ImageConfig, ImageStore,
    MAX_UNPACKED_IMAGE_BYTES, OnFull, PulledImage, Result, STAGING_PREFIX, StagingDir,
    dir_size_bytes, unpack_into, validate_reference,
};
use crate::backend::commit;
use crate::sync::LockUnpoisoned;

const ARCHIVE_FILE: &str = "archive.tar";
const OCI_LAYOUT_FILE: &str = "oci-layout";
const OCI_LAYOUT_BODY: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;
const INDEX_FILE: &str = "index.json";
const DOCKER_MANIFEST_FILE: &str = "manifest.json";
const BLOB_DIR: &str = "blobs/sha256";
const MEDIA_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MEDIA_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const MEDIA_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
const MEDIA_GZIP_SUFFIX: &str = "gzip";
const MEDIA_TAR_SUFFIX: &str = "tar";
const NAME_ANNOTATION: &str = "io.containerd.image.name";
const REF_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const GUEST_OS: &str = "linux";
const SAVED_BY: &str = "ward save";
const MODE_MASK: u32 = 0o7777;
const FILE_MODE: u32 = 0o644;
/// The largest manifest, index or config an archive may hold.
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// A saved archive on disk, removed on drop.
pub struct SavedArchive {
    pub path: PathBuf,
    _staging: StagingDir,
}

/// What `docker import` was given besides the tar.
#[derive(Debug, Clone, Default)]
pub struct ImportSpec {
    /// Local tag for the image; unset leaves it untagged.
    pub reference: Option<String>,
    pub message: String,
    /// Dockerfile instructions applied to the image config.
    pub changes: Vec<String>,
}

/// An image a load added to the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub digest: String,
    /// Empty when the archive gave the image no name.
    pub references: Vec<String>,
}

struct SaveSource {
    meta: CacheMeta,
    rootfs: PathBuf,
    references: Vec<String>,
}

struct Built {
    digest: String,
    manifest: Vec<u8>,
    config_digest: String,
    config: Vec<u8>,
    layer_digest: String,
    layer_path: PathBuf,
    references: Vec<String>,
}

fn bad_archive(why: impl std::fmt::Display) -> BackendError {
    BackendError::InvalidArgument(format!("not a valid image archive: {why}"))
}

fn blob_path(digest: &str) -> String {
    format!(
        "{BLOB_DIR}/{}",
        digest.trim_start_matches(DIGEST_ALGORITHM_PREFIX)
    )
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("{DIGEST_ALGORITHM_PREFIX}{:x}", Sha256::digest(bytes))
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Adds the files under `dir` to `builder` in name order.
fn append_tree<W: Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    dir: &Path,
    skip: &[&str],
) -> io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        let name = path.strip_prefix(root).map_err(io::Error::other)?;
        if skip.contains(&format!("/{}", name.display()).as_str()) {
            continue;
        }
        let kind = meta.file_type();
        let mut header = tar::Header::new_gnu();
        header.set_mode(meta.mode() & MODE_MASK);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(if kind.is_file() {
            meta.mtime().max(0) as u64
        } else {
            0
        });
        if kind.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            builder.append_data(&mut header, name, io::empty())?;
            append_tree(builder, root, &path, skip)?;
        } else if kind.is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder.append_link(&mut header, name, std::fs::read_link(&path)?)?;
        } else if kind.is_file() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(meta.len());
            builder.append_data(&mut header, name, File::open(&path)?)?;
        }
    }
    Ok(())
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    body: &[u8],
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mode(FILE_MODE);
    header.set_size(body.len() as u64);
    builder.append_data(&mut header, name, body)
}

fn architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

fn config_json(
    created: chrono::DateTime<chrono::Utc>,
    runtime: ImageConfig,
    layer_digest: &str,
) -> Vec<u8> {
    let created = created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut config = Map::new();
    for (key, value) in [
        ("Entrypoint", runtime.entrypoint),
        ("Cmd", runtime.cmd),
        ("Env", runtime.env),
    ] {
        if !value.is_empty() {
            config.insert(key.to_string(), json!(value));
        }
    }
    for (key, value) in [("WorkingDir", runtime.working_dir), ("User", runtime.user)] {
        if !value.is_empty() {
            config.insert(key.to_string(), json!(value));
        }
    }
    let config = json!({
        "created": created,
        "architecture": architecture(),
        "os": GUEST_OS,
        "config": config,
        "rootfs": {"type": "layers", "diff_ids": [layer_digest]},
        "history": [{"created": created, "created_by": SAVED_BY}],
    });
    config.to_string().into_bytes()
}

fn build_image(position: usize, source: &SaveSource, scratch: &Path) -> Result<Built> {
    let layer_path = scratch.join(format!("layer-{position}.tar"));
    let mut writer = HashingWriter {
        inner: BufWriter::new(File::create(&layer_path)?),
        hasher: Sha256::new(),
        written: 0,
    };
    let mut builder = tar::Builder::new(&mut writer);
    append_tree(&mut builder, &source.rootfs, &source.rootfs, &[])?;
    builder.finish()?;
    drop(builder);
    writer.flush()?;
    let layer_digest = format!(
        "{DIGEST_ALGORITHM_PREFIX}{:x}",
        writer.hasher.finalize_reset()
    );
    let layer_size = writer.written;

    let config = config_json(
        source.meta.created_at.unwrap_or(source.meta.last_used_at),
        source.meta.config.clone().unwrap_or_default(),
        &layer_digest,
    );
    let config_digest = sha256_of(&config);
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_MANIFEST,
        "config": {"mediaType": MEDIA_CONFIG, "digest": config_digest, "size": config.len()},
        "layers": [{"mediaType": MEDIA_LAYER, "digest": layer_digest, "size": layer_size}],
    })
    .to_string()
    .into_bytes();
    Ok(Built {
        digest: sha256_of(&manifest),
        manifest,
        config_digest,
        config,
        layer_digest,
        layer_path,
        references: source.references.clone(),
    })
}

/// The tag of `reference`, for the OCI `ref.name` annotation. A digest
/// reference has none.
fn tag_of(reference: &str) -> Option<&str> {
    if reference.contains('@') {
        return None;
    }
    Some(
        reference
            .rsplit_once(':')
            .filter(|(_, tag)| !tag.contains('/'))
            .map_or("latest", |(_, tag)| tag),
    )
}

fn index_json(images: &[Built]) -> Vec<u8> {
    let mut manifests = Vec::new();
    for image in images {
        let descriptor = |annotations: Value| {
            json!({
                "mediaType": MEDIA_MANIFEST,
                "digest": image.digest,
                "size": image.manifest.len(),
                "annotations": annotations,
            })
        };
        if image.references.is_empty() {
            manifests.push(descriptor(json!({})));
        }
        for reference in &image.references {
            let mut annotations = Map::new();
            annotations.insert(NAME_ANNOTATION.to_string(), json!(reference));
            if let Some(tag) = tag_of(reference) {
                annotations.insert(REF_ANNOTATION.to_string(), json!(tag));
            }
            manifests.push(descriptor(Value::Object(annotations)));
        }
    }
    json!({"schemaVersion": 2, "mediaType": MEDIA_INDEX, "manifests": manifests})
        .to_string()
        .into_bytes()
}

fn docker_manifest(images: &[Built]) -> Vec<u8> {
    let entries: Vec<Value> = images
        .iter()
        .map(|image| {
            json!({
                "Config": blob_path(&image.config_digest),
                "RepoTags": image.references,
                "Layers": [blob_path(&image.layer_digest)],
            })
        })
        .collect();
    Value::Array(entries).to_string().into_bytes()
}

fn write_archive(sources: &[SaveSource], scratch: &Path, out: &Path) -> Result<()> {
    let images = sources
        .iter()
        .enumerate()
        .map(|(position, source)| build_image(position, source, scratch))
        .collect::<Result<Vec<_>>>()?;
    let mut tar = tar::Builder::new(BufWriter::new(File::create(out)?));
    append_bytes(&mut tar, OCI_LAYOUT_FILE, OCI_LAYOUT_BODY.as_bytes())?;
    append_bytes(&mut tar, INDEX_FILE, &index_json(&images))?;
    append_bytes(&mut tar, DOCKER_MANIFEST_FILE, &docker_manifest(&images))?;
    let mut written = std::collections::HashSet::new();
    for image in &images {
        if written.insert(image.layer_digest.clone()) {
            let mut header = tar::Header::new_gnu();
            header.set_mode(FILE_MODE);
            header.set_size(std::fs::metadata(&image.layer_path)?.len());
            tar.append_data(
                &mut header,
                blob_path(&image.layer_digest),
                File::open(&image.layer_path)?,
            )?;
        }
        append_bytes(&mut tar, &blob_path(&image.config_digest), &image.config)?;
        append_bytes(&mut tar, &blob_path(&image.digest), &image.manifest)?;
    }
    tar.into_inner()?.flush()?;
    Ok(())
}

/// The archive entry name with any leading `./` removed.
fn entry_name<R: Read>(entry: &tar::Entry<'_, R>) -> io::Result<String> {
    let path = entry.path()?;
    Ok(path.to_string_lossy().trim_start_matches("./").to_string())
}

/// The body of the regular file `name` in the archive, if it holds one.
fn read_entry(archive: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let mut tar = tar::Archive::new(File::open(archive)?);
    for entry in tar.entries().map_err(bad_archive)? {
        let mut entry = entry.map_err(bad_archive)?;
        if !entry.header().entry_type().is_file() || entry_name(&entry)? != name {
            continue;
        }
        let mut body = Vec::new();
        (&mut entry)
            .take(MAX_JSON_BYTES + 1)
            .read_to_end(&mut body)?;
        if body.len() as u64 > MAX_JSON_BYTES {
            return Err(bad_archive(format!("{name} is too large")));
        }
        return Ok(Some(body));
    }
    Ok(None)
}

fn require_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    read_entry(archive, name)?.ok_or_else(|| bad_archive(format!("{name} is missing")))
}

fn json_of(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| bad_archive(format!("{what}: {e}")))
}

fn text<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| bad_archive(format!("{what} has no {key}")))
}

fn is_sha256(digest: &str) -> bool {
    digest
        .strip_prefix(DIGEST_ALGORITHM_PREFIX)
        .is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// An image to unpack: where its files are in the archive, and the digests
/// they must have when the archive names them.
struct Spec {
    /// The manifest digest; unset in the Docker format, which has none.
    digest: Option<String>,
    config: (String, Option<String>),
    layers: Vec<(String, Option<String>, u64)>,
    references: Vec<String>,
}

fn oci_specs(archive: &Path, index: &Value) -> Result<Vec<Spec>> {
    let mut specs: Vec<Spec> = Vec::new();
    for descriptor in index["manifests"].as_array().map_or(&[][..], Vec::as_slice) {
        let digest = text(descriptor, "digest", "an index entry")?;
        if !is_sha256(digest) {
            return Err(bad_archive(format!("unsupported digest {digest}")));
        }
        let reference = descriptor["annotations"][NAME_ANNOTATION]
            .as_str()
            .or_else(|| {
                descriptor["annotations"][REF_ANNOTATION]
                    .as_str()
                    .filter(|name| name.contains([':', '/']))
            })
            .map(String::from);
        if let Some(spec) = specs
            .iter_mut()
            .find(|s| s.digest.as_deref() == Some(digest))
        {
            spec.references.extend(reference);
            continue;
        }
        let media = text(descriptor, "mediaType", "an index entry")?;
        if media != MEDIA_MANIFEST && media != MEDIA_DOCKER_MANIFEST {
            return Err(bad_archive(format!("unsupported media type {media}")));
        }
        let bytes = require_entry(archive, &blob_path(digest))?;
        if sha256_of(&bytes) != digest {
            return Err(bad_archive(format!("manifest {digest} is corrupt")));
        }
        let manifest = json_of(&bytes, "manifest")?;
        let config = text(&manifest["config"], "digest", "the manifest config")?;
        let mut layers = Vec::new();
        for layer in manifest["layers"].as_array().map_or(&[][..], Vec::as_slice) {
            let layer_digest = text(layer, "digest", "a layer")?;
            if !is_sha256(layer_digest) {
                return Err(bad_archive(format!("unsupported digest {layer_digest}")));
            }
            layers.push((
                blob_path(layer_digest),
                Some(layer_digest.to_string()),
                layer["size"].as_u64().unwrap_or_default(),
            ));
        }
        if !is_sha256(config) {
            return Err(bad_archive(format!("unsupported digest {config}")));
        }
        specs.push(Spec {
            digest: Some(digest.to_string()),
            config: (blob_path(config), Some(config.to_string())),
            layers,
            references: reference.into_iter().collect(),
        });
    }
    Ok(specs)
}

fn docker_specs(archive: &Path, manifest: &Value) -> Result<Vec<Spec>> {
    let mut specs = Vec::new();
    for entry in manifest.as_array().map_or(&[][..], Vec::as_slice) {
        let mut layers = Vec::new();
        for layer in entry["Layers"].as_array().map_or(&[][..], Vec::as_slice) {
            let path = layer
                .as_str()
                .ok_or_else(|| bad_archive("a layer path is not text"))?;
            let size = layer_size(archive, path)?;
            layers.push((path.trim_start_matches("./").to_string(), None, size));
        }
        specs.push(Spec {
            digest: None,
            config: (text(entry, "Config", "a manifest entry")?.to_string(), None),
            layers,
            references: entry["RepoTags"]
                .as_array()
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(|tag| tag.as_str().map(String::from))
                .collect(),
        });
    }
    Ok(specs)
}

fn layer_size(archive: &Path, name: &str) -> Result<u64> {
    let mut tar = tar::Archive::new(File::open(archive)?);
    for entry in tar.entries().map_err(bad_archive)? {
        let entry = entry.map_err(bad_archive)?;
        if entry_name(&entry)? == name.trim_start_matches("./") {
            return Ok(entry.size());
        }
    }
    Err(bad_archive(format!("layer {name} is missing")))
}

/// Unpacks a tar layer, gzip or plain, into `rootfs`.
fn unpack_auto<R: Read>(source: R, rootfs: &Path) -> Result<()> {
    let mut buffered = BufReader::new(source);
    let media = if buffered.fill_buf()?.starts_with(&GZIP_MAGIC) {
        MEDIA_GZIP_SUFFIX
    } else {
        MEDIA_TAR_SUFFIX
    };
    let limit = MAX_UNPACKED_IMAGE_BYTES.saturating_sub(dir_size_bytes(rootfs)?);
    unpack_into(&mut buffered, media, rootfs, limit)
}

/// Unpacks the layer `name`, checking its digest when one is given.
fn unpack_layer(archive: &Path, name: &str, expected: Option<&str>, rootfs: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(File::open(archive)?);
    for entry in tar.entries().map_err(bad_archive)? {
        let entry = entry.map_err(bad_archive)?;
        if !entry.header().entry_type().is_file() || entry_name(&entry)? != name {
            continue;
        }
        let mut reader = HashingReader {
            inner: entry,
            hasher: Sha256::new(),
        };
        unpack_auto(&mut reader, rootfs)?;
        io::copy(&mut reader, &mut io::sink())?;
        let actual = format!("{DIGEST_ALGORITHM_PREFIX}{:x}", reader.hasher.finalize());
        return match expected {
            Some(wanted) if wanted != actual => {
                Err(bad_archive(format!("layer {wanted} is corrupt")))
            }
            _ => Ok(()),
        };
    }
    Err(bad_archive(format!("layer {name} is missing")))
}

struct Prepared {
    staging: StagingDir,
    pulled: PulledImage,
    references: Vec<String>,
    created: Option<chrono::DateTime<chrono::Utc>>,
}

fn unpack_archive(archive: &Path, cache_dir: &Path) -> Result<Vec<Prepared>> {
    let specs = if let Some(index) = read_entry(archive, INDEX_FILE)? {
        oci_specs(archive, &json_of(&index, INDEX_FILE)?)?
    } else {
        let manifest = require_entry(archive, DOCKER_MANIFEST_FILE)?;
        docker_specs(archive, &json_of(&manifest, DOCKER_MANIFEST_FILE)?)?
    };
    let mut prepared = Vec::new();
    for spec in specs {
        let staging =
            StagingDir(cache_dir.join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())));
        let rootfs = staging.0.join("rootfs");
        std::fs::create_dir_all(&rootfs)?;
        for (name, digest, _) in &spec.layers {
            unpack_layer(archive, name, digest.as_deref(), &rootfs)?;
        }
        let blob = require_entry(archive, &spec.config.0)?;
        if spec
            .config
            .1
            .as_deref()
            .is_some_and(|wanted| wanted != sha256_of(&blob))
        {
            return Err(bad_archive("the image config is corrupt"));
        }
        let sizes: Vec<u64> = spec.layers.iter().map(|(_, _, size)| *size).collect();
        let created = json_of(&blob, "image config")?["created"]
            .as_str()
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.with_timezone(&chrono::Utc));
        for reference in &spec.references {
            validate_reference(reference)?;
        }
        prepared.push(Prepared {
            staging,
            pulled: PulledImage {
                digest: spec.digest.unwrap_or_else(|| sha256_of(&blob)),
                config: ImageConfig::from_blob(&blob)?.with_layer_sizes(&sizes),
            },
            references: spec.references,
            created,
        });
    }
    Ok(prepared)
}

impl ImageStore {
    /// Write the images `names` refer to as one archive. A tag saves only that
    /// tag, an id saves every tag of the image.
    pub async fn save_images(&self, names: &[String]) -> Result<SavedArchive> {
        let mut sources: Vec<SaveSource> = Vec::new();
        for name in names {
            let (dir, meta, matched) = self.require_named(name)?;
            let references: Vec<String> = match matched {
                Matched::Tag(tag) => vec![tag],
                Matched::Id => meta
                    .refs()
                    .filter(|reference| *reference != meta.digest)
                    .map(String::from)
                    .collect(),
            };
            match sources.iter_mut().find(|s| s.meta.digest == meta.digest) {
                Some(known) => known.references.extend(references),
                None => sources.push(SaveSource {
                    rootfs: dir.join("rootfs"),
                    meta,
                    references,
                }),
            }
        }
        let staging = StagingDir(
            self.cache_dir
                .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir_all(&staging.0)?;
        let path = staging.0.join(ARCHIVE_FILE);
        let (scratch, out) = (staging.0.clone(), path.clone());
        tokio::task::spawn_blocking(move || write_archive(&sources, &scratch, &out))
            .await
            .map_err(|e| BackendError::Internal(format!("save task: {e}")))??;
        Ok(SavedArchive {
            path,
            _staging: staging,
        })
    }

    /// Add the images in the archive received on `chunks` to the cache. They
    /// are kept like committed images: nothing can pull them back.
    pub async fn load_images(
        &self,
        chunks: mpsc::Receiver<io::Result<Bytes>>,
    ) -> Result<Vec<LoadedImage>> {
        let staging = StagingDir(
            self.cache_dir
                .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir_all(&staging.0)?;
        let archive = staging.0.join(ARCHIVE_FILE);
        self.spool(chunks, &archive).await?;
        let cache_dir = self.cache_dir.clone();
        let prepared = tokio::task::spawn_blocking(move || unpack_archive(&archive, &cache_dir))
            .await
            .map_err(|e| BackendError::Internal(format!("load task: {e}")))??;
        drop(staging);

        let mut loaded = Vec::new();
        for image in prepared {
            let digest = image.pulled.digest.clone();
            let first = image.references.first().unwrap_or(&digest).clone();
            self.install_entry(image.staging, &first, image.pulled, OnFull::Fail, None)?;
            self.keep_loaded(&digest, &image.references, image.created)?;
            loaded.push(LoadedImage {
                digest,
                references: image.references,
            });
        }
        Ok(loaded)
    }

    /// Write an uploaded stream to `path`, refusing more than the pull cap.
    async fn spool(
        &self,
        mut chunks: mpsc::Receiver<io::Result<Bytes>>,
        path: &Path,
    ) -> Result<()> {
        let mut file = tokio::fs::File::create(path).await?;
        let mut received = 0u64;
        while let Some(chunk) = chunks.recv().await {
            let chunk = chunk
                .map_err(|e| BackendError::InvalidArgument(format!("the upload failed: {e}")))?;
            received += chunk.len() as u64;
            if received > self.max_pull_bytes {
                return Err(BackendError::ResourceExhausted(format!(
                    "the archive is larger than {} bytes",
                    self.max_pull_bytes
                )));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Write the files of a sandbox root filesystem as a tar archive, like
    /// `docker export`. `skip` holds guest paths the backend wrote itself.
    pub async fn export_rootfs(
        &self,
        rootfs: &Path,
        skip: &'static [&'static str],
    ) -> Result<SavedArchive> {
        let staging = self.staging_dir()?;
        let path = staging.0.join(ARCHIVE_FILE);
        let (rootfs, out) = (rootfs.to_path_buf(), path.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut tar = tar::Builder::new(BufWriter::new(File::create(out)?));
            append_tree(&mut tar, &rootfs, &rootfs, skip)?;
            tar.into_inner()?.flush()?;
            Ok(())
        })
        .await
        .map_err(|e| BackendError::Internal(format!("export task: {e}")))??;
        Ok(SavedArchive {
            path,
            _staging: staging,
        })
    }

    /// Make a one-layer image from a tar stream, like `docker import`. The
    /// image is kept like a committed one.
    pub async fn import_image(
        &self,
        spec: ImportSpec,
        chunks: mpsc::Receiver<io::Result<Bytes>>,
    ) -> Result<LoadedImage> {
        if let Some(reference) = &spec.reference {
            validate_reference(reference)?;
        }
        let mut config = ImageConfig::default();
        commit::apply_changes(&mut config, &spec.changes)?;
        let staging = self.staging_dir()?;
        let archive = staging.0.join(ARCHIVE_FILE);
        self.spool(chunks, &archive).await?;
        let rootfs = staging.0.join("rootfs");
        let layer_digest = tokio::task::spawn_blocking(move || -> Result<String> {
            std::fs::create_dir_all(&rootfs)?;
            let mut reader = HashingReader {
                inner: File::open(&archive)?,
                hasher: Sha256::new(),
            };
            unpack_auto(&mut reader, &rootfs).map_err(|e| match e {
                BackendError::Internal(_) => {
                    BackendError::InvalidArgument("the file is not a tar archive".to_string())
                }
                other => other,
            })?;
            io::copy(&mut reader, &mut io::sink())?;
            std::fs::remove_file(&archive)?;
            Ok(format!(
                "{DIGEST_ALGORITHM_PREFIX}{:x}",
                reader.hasher.finalize()
            ))
        })
        .await
        .map_err(|e| BackendError::Internal(format!("import task: {e}")))??;

        let now = chrono::Utc::now();
        config.history.push(HistoryEntry {
            created: Some(now),
            created_by: String::new(),
            comment: spec.message,
            empty_layer: false,
            size_bytes: 0,
        });
        let digest = sha256_of(&config_json(now, config.clone(), &layer_digest));
        let first = spec.reference.clone().unwrap_or_else(|| digest.clone());
        let pulled = PulledImage {
            digest: digest.clone(),
            config,
        };
        self.install_entry(staging, &first, pulled, OnFull::Fail, None)?;
        let references: Vec<String> = spec.reference.into_iter().collect();
        self.keep_loaded(&digest, &references, Some(now))?;
        Ok(LoadedImage { digest, references })
    }

    fn staging_dir(&self) -> Result<StagingDir> {
        let staging = StagingDir(
            self.cache_dir
                .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir_all(&staging.0)?;
        Ok(staging)
    }

    /// Give the loaded image all its names, its original creation time, and
    /// protection from eviction.
    fn keep_loaded(
        &self,
        digest: &str,
        references: &[String],
        created: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        let _guard = self.install_lock.lock_unpoisoned();
        let Some((dir, mut meta)) = self.find_cache_entry_by_ref(digest)? else {
            return Ok(());
        };
        for reference in references {
            self.untag_others(reference, digest)?;
            meta.add_ref(reference);
        }
        meta.committed = true;
        meta.created_at = created.or(meta.created_at);
        super::write_meta_json(&dir.join("meta.json"), &meta)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Arc;

    use super::*;
    use crate::backend::image::ImagePuller;

    const REFERENCE: &str = "app:v1";
    const SCRIPT_MODE: u32 = 0o755;

    #[derive(Debug)]
    struct FilesPuller;

    #[async_trait::async_trait]
    impl ImagePuller for FilesPuller {
        async fn pull(&self, _reference: &str, dest: &Path) -> Result<PulledImage> {
            std::fs::create_dir_all(dest.join("usr/bin"))?;
            std::fs::write(dest.join("usr/bin/run.sh"), "#!/bin/sh\n")?;
            std::fs::set_permissions(
                dest.join("usr/bin/run.sh"),
                std::fs::Permissions::from_mode(SCRIPT_MODE),
            )?;
            std::fs::write(dest.join("etc.conf"), "k=v\n")?;
            symlink("usr/bin/run.sh", dest.join("run"))?;
            let mut image = PulledImage::without_config(format!("sha256:{}", "cd".repeat(32)));
            image.config.cmd = vec!["/run".to_string()];
            image.config.env = vec!["A=1".to_string()];
            Ok(image)
        }
    }

    async fn store() -> (tempfile::TempDir, ImageStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ImageStore::with_puller(dir.path().to_path_buf(), 64, Arc::new(FilesPuller));
        (dir, store)
    }

    async fn upload(path: &Path) -> mpsc::Receiver<io::Result<Bytes>> {
        let (tx, rx) = mpsc::channel(1);
        let body = std::fs::read(path).expect("read archive");
        tokio::spawn(async move {
            for chunk in body.chunks(1000) {
                let _ = tx.send(Ok(Bytes::copy_from_slice(chunk))).await;
            }
        });
        rx
    }

    async fn round_trip(from: &ImageStore, names: &[&str], to: &ImageStore) -> LoadedImage {
        let names: Vec<String> = names.iter().map(ToString::to_string).collect();
        let archive = from.save_images(&names).await.expect("save");
        let mut loaded = to
            .load_images(upload(&archive.path).await)
            .await
            .expect("load");
        assert_eq!(loaded.len(), 1);
        loaded.remove(0)
    }

    #[tokio::test]
    async fn given_a_saved_image_when_loaded_elsewhere_then_files_config_and_name_survive() {
        // Arrange
        let (_a, source) = store().await;
        source.pull(REFERENCE, None).await.expect("pull");
        let (_b, target) = store().await;

        // Act
        let loaded = round_trip(&source, &[REFERENCE], &target).await;

        // Assert
        assert_eq!(loaded.references, [REFERENCE]);
        let rootfs = target
            .cached_rootfs(REFERENCE)
            .await
            .expect("lookup")
            .expect("cached");
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc.conf")).expect("read"),
            "k=v\n"
        );
        let mode = std::fs::metadata(rootfs.join("usr/bin/run.sh"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, SCRIPT_MODE);
        assert_eq!(
            std::fs::read_link(rootfs.join("run")).expect("link"),
            Path::new("usr/bin/run.sh")
        );
        let config = target.config(REFERENCE).await.expect("config");
        assert_eq!(config.cmd, ["/run"]);
        assert_eq!(config.env, ["A=1"]);
    }

    #[tokio::test]
    async fn given_a_loaded_image_when_saved_and_loaded_again_then_the_digest_is_the_same() {
        // Arrange
        let (_a, source) = store().await;
        source.pull(REFERENCE, None).await.expect("pull");
        let (_b, first) = store().await;
        let (_c, second) = store().await;

        // Act
        let once = round_trip(&source, &[REFERENCE], &first).await;
        let twice = round_trip(&first, &[REFERENCE], &second).await;

        // Assert
        assert_eq!(once.digest, twice.digest);
        assert_ne!(once.digest, format!("sha256:{}", "cd".repeat(32)));
    }

    #[tokio::test]
    async fn given_an_image_with_two_tags_when_saved_by_id_then_both_tags_load() {
        // Arrange
        let (_a, source) = store().await;
        source.pull(REFERENCE, None).await.expect("pull");
        source.tag_image(REFERENCE, "app:stable").expect("tag");
        let (_b, target) = store().await;

        // Act
        let loaded = round_trip(&source, &["cdcd"], &target).await;

        // Assert
        let mut references = loaded.references;
        references.sort();
        assert_eq!(references, ["app:stable", "app:v1"]);
    }

    #[tokio::test]
    async fn given_a_tampered_layer_when_loaded_then_it_is_refused_and_nothing_is_cached() {
        // Arrange
        let (_a, source) = store().await;
        source.pull(REFERENCE, None).await.expect("pull");
        let archive = source
            .save_images(&[REFERENCE.to_string()])
            .await
            .expect("save");
        let mut body = std::fs::read(&archive.path).expect("read");
        let at = body
            .windows(4)
            .position(|w| w == b"k=v\n")
            .expect("file body");
        body[at] = b'X';
        let broken = archive.path.with_extension("broken");
        std::fs::write(&broken, body).expect("write");
        let (_b, target) = store().await;

        // Act
        let error = target
            .load_images(upload(&broken).await)
            .await
            .expect_err("tampered");

        // Assert
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert!(target.list().await.is_empty());
    }

    #[tokio::test]
    async fn given_bytes_that_are_not_an_archive_when_loaded_then_it_is_refused() {
        // Arrange
        let (_dir, target) = store().await;
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"not a tar file")))
            .await
            .expect("send");
        drop(tx);

        // Act
        let error = target.load_images(rx).await.expect_err("refused");

        // Assert
        assert!(matches!(error, BackendError::InvalidArgument(_)), "{error}");
    }

    #[tokio::test]
    async fn given_an_exported_tree_when_imported_then_it_is_a_tagged_image_with_the_changes() {
        // Arrange
        let (_a, source) = store().await;
        source.pull(REFERENCE, None).await.expect("pull");
        let rootfs = source
            .cached_rootfs(REFERENCE)
            .await
            .expect("lookup")
            .expect("cached");
        std::fs::write(rootfs.join(".agent"), "boot").expect("write");
        let exported = source
            .export_rootfs(&rootfs, &["/.agent"])
            .await
            .expect("export");
        let (_b, target) = store().await;
        let spec = ImportSpec {
            reference: Some("imported:v1".to_string()),
            message: "from a tar".to_string(),
            changes: vec!["CMD [\"/run\"]".to_string()],
        };

        // Act
        let imported = target
            .import_image(spec, upload(&exported.path).await)
            .await
            .expect("import");

        // Assert
        assert_eq!(imported.references, ["imported:v1"]);
        let copy = target
            .cached_rootfs("imported:v1")
            .await
            .expect("lookup")
            .expect("cached");
        assert_eq!(
            std::fs::read_to_string(copy.join("etc.conf")).expect("read"),
            "k=v\n"
        );
        assert!(!copy.join(".agent").exists());
        let view = target.image_history("imported:v1").expect("history");
        assert_eq!(view.entries[0].comment, "from a tar");
        assert_eq!(
            target.config("imported:v1").await.expect("config").cmd,
            ["/run"]
        );
    }

    #[tokio::test]
    async fn given_bytes_that_are_not_a_tar_when_imported_then_it_is_refused() {
        // Arrange
        let (_dir, target) = store().await;
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(
            b"plain text, not a tar archive at all",
        )))
        .await
        .expect("send");
        drop(tx);

        // Act
        let error = target
            .import_image(ImportSpec::default(), rx)
            .await
            .expect_err("refused");

        // Assert
        assert!(matches!(error, BackendError::InvalidArgument(_)), "{error}");
        assert!(target.list().await.is_empty());
    }
}
