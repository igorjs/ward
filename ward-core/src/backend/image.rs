// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! OCI image pull, unpack, and disk-persistent content-addressable cache.
//!
//! # Cache layout
//!
//! ```text
//! <cache_dir>/
//!   sha256-<hex>/
//!     rootfs/    (unpacked OCI layers, content-addressable by manifest digest)
//!     meta.json  (image_ref, digest, size_bytes, last_used_at, layer_count)
//! ```
//!
//! The cache key is the manifest digest (not the image tag), so two tags that
//! resolve to the same image share one cache entry. `last_used_at` in
//! `meta.json` drives LRU eviction when the entry count exceeds
//! `max_cached_images`.
//!
//! # Materialization
//!
//! `prepare_sandbox_rootfs` copies the cached rootfs into the per-sandbox
//! directory file by file. Files are copied, not hardlinked: the guest writes
//! to its rootfs in place, and a shared inode would let one sandbox change
//! the image cache and every later sandbox built from it.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flate2::read::GzDecoder;
use oci_client::errors::{OciDistributionError, OciErrorCode};
use std::io::ErrorKind::FileTooLarge;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::{Stream, StreamExt};

use super::commit::{self, CommitSpec, CopyError, CopyLimits};
use super::{BackendError, Result};
use crate::pb::PullPolicy;
use crate::sync::LockUnpoisoned;

mod archive;
mod manage;

pub use archive::{ImportSpec, LoadedImage, SavedArchive};
pub use manage::{HistoryView, ImageRemoval};

const GIB: u64 = 1024 * 1024 * 1024;
/// A pull stops once the layers it downloads add up to this many bytes.
pub const MAX_PULL_BYTES: u64 = 2 * GIB;
/// A pull fails rather than grow the whole image cache past this many bytes.
pub const MAX_CACHE_BYTES: u64 = 20 * GIB;
/// Docker shows layers by the first characters of their digest.
const LAYER_ID_LEN: usize = 12;
const DIGEST_ALGORITHM_PREFIX: &str = "sha256:";

/// Set to `1` to skip registry pulls and build synthetic, unbootable images.
pub(crate) const OFFLINE_IMAGES_ENV: &str = "WARD_OCI_OFFLINE";

// ---------------------------------------------------------------------------
// Cached image entry (returned by list())
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
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Cache metadata persisted in meta.json
// ---------------------------------------------------------------------------

/// JSON record written alongside each cached rootfs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheMeta {
    pub image_ref: String,
    pub digest: String,
    pub size_bytes: u64,
    pub last_used_at: chrono::DateTime<chrono::Utc>,
    pub layer_count: usize,
    /// `None` for entries cached before the config was kept.
    #[serde(default)]
    pub config: Option<ImageConfig>,
    /// When the image was first cached. `None` for older entries, which
    /// fall back to `last_used_at`.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Who committed the image. `None` for pulled images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// The commit message. `None` for pulled images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Made by a commit. Such an image is never evicted from the cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub committed: bool,
    /// Other references naming this image besides `image_ref`, e.g. a second
    /// tag that resolved to the same digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl CacheMeta {
    fn has_ref(&self, reference: &str) -> bool {
        self.digest == reference
            || self.image_ref == reference
            || self.aliases.iter().any(|alias| alias == reference)
    }

    /// Whether the image has no reference of its own and is named by digest.
    fn is_untagged(&self) -> bool {
        self.image_ref == self.digest
    }

    /// Every reference naming the image, or its digest when it has none.
    fn refs(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.image_ref.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    fn add_ref(&mut self, reference: &str) {
        if self.has_ref(reference) {
            return;
        }
        if self.is_untagged() {
            self.image_ref = reference.to_string();
        } else {
            self.aliases.push(reference.to_string());
        }
    }

    /// The same image without `reference`. An image left with no reference
    /// is named by its digest.
    fn without_ref(&self, reference: &str) -> Self {
        let mut meta = self.clone();
        meta.aliases.retain(|alias| alias != reference);
        if meta.image_ref == reference {
            meta.image_ref = if meta.aliases.is_empty() {
                meta.digest.clone()
            } else {
                meta.aliases.remove(0)
            };
        }
        meta
    }
}

/// The parts of an OCI image config a sandbox's main process starts from.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub working_dir: String,
    /// Recorded for later use: the guest does not switch user yet.
    #[serde(default)]
    pub user: String,
    /// `KEY=VALUE` entries.
    #[serde(default)]
    pub env: Vec<String>,
    /// Build steps, oldest first.
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

/// One build step of an image, as `docker history` lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    #[serde(default)]
    pub created: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub comment: String,
    /// The step changed only the config and added no layer.
    #[serde(default)]
    pub empty_layer: bool,
    /// Size of the layer the step added; zero when it added none.
    #[serde(default)]
    pub size_bytes: u64,
}

/// A `history` entry of an OCI image config. A registry's `created` is
/// free text, so one that does not parse is dropped, not fatal.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RawHistoryEntry {
    created: Option<String>,
    created_by: Option<String>,
    comment: Option<String>,
    empty_layer: bool,
}

impl From<RawHistoryEntry> for HistoryEntry {
    fn from(raw: RawHistoryEntry) -> Self {
        Self {
            created: raw
                .created
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(&text).ok())
                .map(|time| time.with_timezone(&chrono::Utc)),
            created_by: raw.created_by.unwrap_or_default(),
            comment: raw.comment.unwrap_or_default(),
            empty_layer: raw.empty_layer,
            size_bytes: 0,
        }
    }
}

/// The `config` object of an OCI image config blob. Registries send `null`
/// for unset lists.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawRuntimeConfig {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    working_dir: Option<String>,
    user: Option<String>,
    env: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RawImageConfigBlob {
    config: Option<RawRuntimeConfig>,
    history: Option<Vec<RawHistoryEntry>>,
}

impl ImageConfig {
    /// Read the runtime config out of an OCI image config blob.
    pub fn from_blob(blob: &[u8]) -> Result<Self> {
        let parsed: RawImageConfigBlob = serde_json::from_slice(blob)
            .map_err(|e| BackendError::Image(format!("image config is not valid JSON: {e}")))?;
        let raw = parsed.config.unwrap_or_default();
        Ok(Self {
            entrypoint: raw.entrypoint.unwrap_or_default(),
            cmd: raw.cmd.unwrap_or_default(),
            working_dir: raw.working_dir.unwrap_or_default(),
            user: raw.user.unwrap_or_default(),
            env: raw.env.unwrap_or_default(),
            history: parsed
                .history
                .unwrap_or_default()
                .into_iter()
                .map(HistoryEntry::from)
                .collect(),
        })
    }

    /// Give the steps that added a layer the sizes of the manifest's layers,
    /// oldest first.
    #[must_use]
    pub fn with_layer_sizes(mut self, sizes: &[u64]) -> Self {
        let mut sizes = sizes.iter();
        for entry in self.history.iter_mut().filter(|entry| !entry.empty_layer) {
            entry.size_bytes = sizes.next().copied().unwrap_or_default();
        }
        self
    }
}

/// What a pull produced: the manifest digest and the image's runtime config.
#[derive(Debug, Clone)]
pub struct PulledImage {
    pub digest: String,
    pub config: ImageConfig,
}

impl PulledImage {
    pub fn without_config(digest: String) -> Self {
        Self {
            digest,
            config: ImageConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// ImagePuller
// ---------------------------------------------------------------------------

/// Pulls an image reference and materialises its rootfs into `dest`,
/// returning the manifest digest and the image's runtime config.
///
/// This is the seam between the cache bookkeeping (`ImageStore`) and the
/// registry protocol (`OciPuller`). Tests inject a fake so cache, list,
/// remove, and traversal behaviour can be exercised offline.
#[async_trait::async_trait]
pub trait ImagePuller: Send + Sync + std::fmt::Debug {
    async fn pull(&self, reference: &str, dest: &Path) -> Result<PulledImage>;

    /// Like [`pull`](Self::pull), but reports the bytes it will download to
    /// `session` before downloading them, and its progress as it goes.
    async fn pull_streaming(
        &self,
        reference: &str,
        dest: &Path,
        _session: &mut PullSession,
    ) -> Result<PulledImage> {
        self.pull(reference, dest).await
    }

    /// The digest `reference` points to in the registry right now, fetched
    /// without downloading layers. `None` when the puller cannot tell, so
    /// the caller pulls to find out.
    async fn resolve_digest(&self, _reference: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// One progress line of a pull, in Docker's wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullProgress {
    /// The layer or tag the line is about. Empty for a line about the image.
    pub id: String,
    pub status: String,
}

/// The limits and progress sink of one pull.
#[derive(Debug)]
pub struct PullSession {
    max_bytes: u64,
    cache_room: u64,
    used: u64,
    progress: Option<UnboundedSender<PullProgress>>,
}

impl PullSession {
    pub fn new(
        max_bytes: u64,
        cache_room: u64,
        progress: Option<UnboundedSender<PullProgress>>,
    ) -> Self {
        Self {
            max_bytes,
            cache_room,
            used: 0,
            progress,
        }
    }

    /// Count `bytes` about to be downloaded, failing once the pull passes
    /// its size cap or the room left in the cache.
    pub fn charge(&mut self, bytes: u64) -> Result<()> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.max_bytes {
            return Err(BackendError::ResourceExhausted(format!(
                "image layers exceed the pull limit of {} bytes",
                self.max_bytes
            )));
        }
        self.check_cache_room(self.used)
    }

    fn check_cache_room(&self, bytes: u64) -> Result<()> {
        if bytes > self.cache_room {
            return Err(BackendError::ResourceExhausted(format!(
                "image cache is full: {bytes} bytes do not fit in the {} bytes left",
                self.cache_room
            )));
        }
        Ok(())
    }

    pub fn report(&self, id: &str, status: &str) {
        if let Some(progress) = &self.progress {
            // A client that hung up must not abort a pull half way.
            let _ = progress.send(PullProgress {
                id: id.to_string(),
                status: status.to_string(),
            });
        }
    }
}

/// Real puller: talks to an OCI registry over HTTPS, then unpacks each
/// layer tarball into the destination rootfs.
#[derive(Debug, Default)]
pub struct OciPuller {
    /// `host:port` registries reached over plain HTTP, for local test registries.
    http_registries: Vec<String>,
}

impl OciPuller {
    /// A puller that reaches `registry` (`host:port`) over plain HTTP.
    pub fn with_http_registry(registry: impl Into<String>) -> Self {
        Self {
            http_registries: vec![registry.into()],
        }
    }

    /// Parse `reference`, enforce the registry allowlist and build a client.
    fn client_for(&self, reference: &str) -> Result<(oci_client::Client, oci_client::Reference)> {
        let image_ref: oci_client::Reference = reference.parse().map_err(|e| {
            BackendError::Image(format!("invalid image reference {reference}: {e}"))
        })?;
        check_registry_allowlist(&image_ref)?;
        let client = oci_client::Client::new(oci_client::client::ClientConfig {
            platform_resolver: Some(Box::new(linux_host_arch_resolver)),
            protocol: oci_client::client::ClientProtocol::HttpsExcept(self.http_registries.clone()),
            ..Default::default()
        });
        Ok((client, image_ref))
    }
}

#[async_trait::async_trait]
impl ImagePuller for OciPuller {
    async fn pull(&self, reference: &str, dest: &Path) -> Result<PulledImage> {
        let mut unlimited = PullSession::new(u64::MAX, u64::MAX, None);
        self.pull_streaming(reference, dest, &mut unlimited).await
    }

    async fn pull_streaming(
        &self,
        reference: &str,
        dest: &Path,
        session: &mut PullSession,
    ) -> Result<PulledImage> {
        // WARD_OCI_OFFLINE=1: skip the real registry pull and materialise a
        // minimal synthetic rootfs (a `bin/` directory). Set by e2e test
        // harnesses that spawn wardd in an egress-blocked environment (e.g.
        // CI with step-security/harden-runner where docker.io is not in the
        // allowed-endpoints list).
        //
        // Gated to debug builds so the escape hatch is stripped out of
        // release binaries entirely. Tests run with debug_assertions on,
        // production `cargo build --release` does not see this branch.
        #[cfg(debug_assertions)]
        if std::env::var(OFFLINE_IMAGES_ENV).as_deref() == Ok("1") {
            std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
            return Ok(PulledImage {
                digest: offline_digest(reference),
                config: offline_image_config()?,
            });
        }

        use oci_client::manifest;
        use oci_client::secrets::RegistryAuth;

        let (client, image_ref) = self.client_for(reference)?;
        let auth = RegistryAuth::Anonymous;
        let accepted = [
            manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
            manifest::IMAGE_LAYER_MEDIA_TYPE,
        ];

        let (image_manifest, digest, config_blob) = with_retry(reference, || {
            client.pull_manifest_and_config(&image_ref, &auth)
        })
        .await?;
        let layer_sizes: Vec<u64> = image_manifest
            .layers
            .iter()
            .map(|layer| u64::try_from(layer.size).unwrap_or_default())
            .collect();
        let config = ImageConfig::from_blob(config_blob.as_bytes())?.with_layer_sizes(&layer_sizes);

        if let Some(layer) = image_manifest
            .layers
            .iter()
            .find(|layer| !accepted.contains(&layer.media_type.as_str()))
        {
            return Err(BackendError::Image(format!(
                "unsupported layer media type {}",
                layer.media_type
            )));
        }
        // Charge every layer before the first byte is downloaded, so an
        // image that is too big costs no bandwidth.
        for layer in &image_manifest.layers {
            session.charge(u64::try_from(layer.size).unwrap_or(u64::MAX))?;
        }

        // Layers are applied bottom-up in manifest order; each is a tar
        // diff over the accumulated filesystem.
        for layer in &image_manifest.layers {
            let id = layer_id(&layer.digest);
            session.report(&id, "Pulling fs layer");
            let blob = dest.with_file_name(format!("{id}.blob"));
            with_retry(reference, || async {
                let stream = client.pull_blob_stream(&image_ref, layer).await?;
                Ok(spool(stream, &blob, u64::try_from(layer.size).unwrap_or(0)).await?)
            })
            .await?;
            session.report(&id, "Download complete");
            let media_type = layer.media_type.clone();
            let dest = dest.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let limit = MAX_UNPACKED_IMAGE_BYTES.saturating_sub(dir_size_bytes(&dest)?);
                unpack_into(std::fs::File::open(&blob)?, &media_type, &dest, limit)?;
                std::fs::remove_file(&blob)?;
                Ok::<_, BackendError>(())
            })
            .await
            .map_err(|e| BackendError::Internal(format!("unpack thread panicked: {e}")))??;
            session.report(&id, "Pull complete");
        }

        Ok(PulledImage { digest, config })
    }

    async fn resolve_digest(&self, reference: &str) -> Result<Option<String>> {
        #[cfg(debug_assertions)]
        if std::env::var(OFFLINE_IMAGES_ENV).as_deref() == Ok("1") {
            return Ok(Some(offline_digest(reference)));
        }

        use oci_client::manifest::OciManifest;
        let (client, image_ref) = self.client_for(reference)?;
        let (manifest, digest) = with_retry(reference, || {
            client.pull_manifest(&image_ref, &oci_client::secrets::RegistryAuth::Anonymous)
        })
        .await?;
        // An index points at one manifest per platform: the pull stores the
        // one for this host, so that is the digest to compare.
        Ok(match manifest {
            OciManifest::Image(_) => Some(digest),
            OciManifest::ImageIndex(index) => linux_host_arch_resolver(&index.manifests),
        })
    }
}

/// Refuse a registry that `WARD_REGISTRY_ALLOWLIST` does not list.
fn check_registry_allowlist(image_ref: &oci_client::Reference) -> Result<()> {
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
        tracing::warn!(
            registry = image_ref.registry(),
            allowlist = %raw,
            "registry is not in WARD_REGISTRY_ALLOWLIST"
        );
        return Err(BackendError::Image(format!(
            "registry {} is not allowed by this daemon's registry allowlist",
            image_ref.registry()
        )));
    }

    Ok(())
}

/// Write `stream` to `path`, refusing more than `declared` bytes.
async fn spool(
    mut stream: impl Stream<Item = std::io::Result<bytes::Bytes>> + Unpin,
    path: &Path,
    declared: u64,
) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut left = declared;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        left = left.checked_sub(chunk.len() as u64).ok_or(FileTooLarge)?;
        file.write_all(&chunk).await?;
    }
    file.flush().await
}

/// Parse `reference` as an image reference. It is the only check that runs
/// before a registry is contacted.
pub fn validate_reference(reference: &str) -> Result<oci_client::Reference> {
    reference.parse().map_err(|_| {
        BackendError::InvalidArgument(format!("invalid reference format: {reference}"))
    })
}

/// Whether `reference` names its image by digest, so it cannot move.
fn is_pinned(reference: &str) -> bool {
    reference.contains(&format!("@{DIGEST_ALGORITHM_PREFIX}"))
}

/// The first characters of a layer digest, as Docker shows them.
fn layer_id(digest: &str) -> String {
    digest
        .strip_prefix(DIGEST_ALGORITHM_PREFIX)
        .unwrap_or(digest)
        .chars()
        .take(LAYER_ID_LEN)
        .collect()
}

/// A stable fake digest derived from the reference, so distinct references
/// get distinct cache entries in offline mode.
#[cfg(debug_assertions)]
fn offline_digest(reference: &str) -> String {
    let hash: u64 = reference
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    format!("sha256:{hash:016x}")
}

/// Debug builds only: JSON `ImageConfig` that offline pulls report, so tests
/// can give a synthetic image a command. Unset means an empty config.
#[cfg(debug_assertions)]
const OFFLINE_CONFIG_ENV: &str = "WARD_OCI_OFFLINE_CONFIG";

#[cfg(debug_assertions)]
fn offline_image_config() -> Result<ImageConfig> {
    match std::env::var(OFFLINE_CONFIG_ENV) {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| BackendError::Image(format!("{OFFLINE_CONFIG_ENV} is invalid: {e}"))),
        Err(_) => Ok(ImageConfig::default()),
    }
}

/// Attempts for one registry pull. Registries drop or throttle anonymous
/// requests, and the failure surfaces as an opaque decode error.
const PULL_ATTEMPTS: u32 = 3;
const PULL_RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

async fn with_retry<T, F, Fut>(reference: &str, mut attempt_once: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = std::result::Result<T, oci_client::errors::OciDistributionError>,
        >,
{
    let mut attempt = 1;
    loop {
        match attempt_once().await {
            Ok(value) => return Ok(value),
            Err(e) if attempt < PULL_ATTEMPTS && is_unreachable(&e) => {
                let delay = PULL_RETRY_BASE_DELAY * attempt;
                tracing::warn!(
                    reference,
                    attempt,
                    "image pull failed, retrying in {delay:?}: {e}"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(pull_error(reference, e)),
        }
    }
}

/// Whether the registry said the manifest or repository does not exist.
fn is_unknown_image(e: &OciDistributionError) -> bool {
    use oci_client::errors::OciErrorCode::{ManifestUnknown, NameUnknown, NotFound};
    match e {
        OciDistributionError::ImageManifestNotFoundError(_) => true,
        OciDistributionError::ServerError { code, .. } => *code == NOT_FOUND_STATUS,
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|e| matches!(e.code, ManifestUnknown | NameUnknown | NotFound)),
        _ => false,
    }
}

/// Registries answer an anonymous pull of a missing or private repository
/// with an authorisation failure, which Docker reports as access denied.
fn is_access_denied(e: &OciDistributionError) -> bool {
    matches!(
        e,
        OciDistributionError::UnauthorizedError { .. }
            | OciDistributionError::AuthenticationFailure(_)
    )
}

/// Whether the registry or the network failed to answer, or asked the client
/// to slow down. Only these are worth retrying.
fn is_unreachable(e: &OciDistributionError) -> bool {
    match e {
        OciDistributionError::RequestError(_) => true,
        OciDistributionError::IoError(io) => io.kind() != FileTooLarge,
        OciDistributionError::ServerError { code, .. } => {
            *code >= SERVER_ERROR_STATUS || *code == TOO_MANY_REQUESTS_STATUS
        }
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|e| e.code == OciErrorCode::Toomanyrequests),
        _ => false,
    }
}

const NOT_FOUND_STATUS: u16 = 404;
const SERVER_ERROR_STATUS: u16 = 500;
const TOO_MANY_REQUESTS_STATUS: u16 = 429;

/// Translate a registry failure into the backend error that picks the gRPC
/// code, in Docker's wording where Docker has one.
fn pull_error(reference: &str, e: OciDistributionError) -> BackendError {
    // The source error can carry request URLs, so it stays in the log.
    tracing::warn!(reference, error = %e, "image pull failed");
    if is_oversized(&e) {
        BackendError::ResourceExhausted(format!(
            "a layer of {reference} is larger than its manifest declares"
        ))
    } else if is_unknown_image(&e) {
        BackendError::ImageNotFound(format!(
            "manifest for {reference} not found: manifest unknown"
        ))
    } else if is_access_denied(&e) {
        BackendError::ImageNotFound(format!(
            "pull access denied for {reference}, repository does not exist \
             or may require 'docker login'"
        ))
    } else if is_unreachable(&e) {
        BackendError::Unavailable(format!("could not reach the registry for {reference}"))
    } else {
        BackendError::Internal(format!(
            "pull {reference} failed: unexpected registry response"
        ))
    }
}

/// The registry streamed past the declared size; retrying cannot help.
fn is_oversized(e: &OciDistributionError) -> bool {
    matches!(e, OciDistributionError::IoError(io) if io.kind() == FileTooLarge)
}

/// OCI operating system name of every ward guest.
const GUEST_OS: &str = "linux";

/// Pick the Linux image variant that matches the host CPU architecture.
///
/// The guest is always Linux, even when the host is macOS, so the client's
/// default resolver (host OS and arch) finds no match on a Mac.
fn linux_host_arch_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    let want_arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().is_some_and(|p| {
                p.os.to_string() == GUEST_OS && p.architecture.to_string() == want_arch
            })
        })
        .map(|entry| entry.digest.clone())
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

/// Maximum decompressed bytes across all layers of one image. Prevents
/// zip-bomb attacks where a small compressed layer expands without bound.
/// 10 GiB is well above any realistic image; real base images are < 1 GiB.
const MAX_UNPACKED_IMAGE_BYTES: u64 = 10 * GIB;

/// Wraps a `Read` and errors if the cumulative bytes read exceed `limit`.
struct BoundedReader<R> {
    inner: R,
    limit: u64,
    total: u64,
}

impl<R: Read> BoundedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            total: 0,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        if self.total > self.limit {
            return Err(std::io::Error::new(FileTooLarge, unpack_limit_message()));
        }
        Ok(n)
    }
}

/// Unpack one image layer tarball into `dest`, applying OCI whiteouts.
///
/// `media_type` selects gzip vs. plain tar. The `tar` crate refuses to write
/// outside `dest` and whiteouts go through [`confined_path`]. The layer may
/// unpack to at most `limit` bytes.
fn unpack_into<'a, R: Read + 'a>(
    layer: R,
    media_type: &str,
    dest: &Path,
    limit: u64,
) -> Result<()> {
    let reader: Box<dyn Read + 'a> = if media_type.ends_with("gzip") {
        Box::new(BoundedReader::new(GzDecoder::new(layer), limit))
    } else {
        Box::new(BoundedReader::new(layer, limit))
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
        .map_err(|e| unpack_error("read layer tar", e))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| unpack_error("read tar entry", e))?;
        let path = entry
            .path()
            .map_err(|e| unpack_error("decode tar entry path", e))?
            .into_owned();
        // `/etc/.wh.foo` and `./etc/.wh.foo` name the same file as `etc/.wh.foo`.
        let path: PathBuf = path
            .components()
            .filter(|c| !matches!(c, Component::RootDir | Component::CurDir))
            .collect();

        match classify_entry(&path) {
            EntryKind::OpaqueWhiteout(dir) => apply_opaque_whiteout(dest, &dir)?,
            EntryKind::Whiteout(target) => apply_whiteout(dest, &target)?,
            EntryKind::Normal => {
                entry
                    .unpack_in(dest)
                    .map_err(|e| unpack_error("unpack tar entry", e))?;
            }
        }
    }
    Ok(())
}

/// A failed unpack. Hitting the unpacked-size cap is a resource limit, not a
/// bad image.
fn unpack_error(context: &str, e: std::io::Error) -> BackendError {
    if e.kind() == FileTooLarge {
        BackendError::ResourceExhausted(unpack_limit_message())
    } else {
        // The source error can name host paths, so it stays in the log.
        tracing::warn!(context, error = %e, "layer unpack failed");
        BackendError::Internal(format!("{context} failed"))
    }
}

fn unpack_limit_message() -> String {
    format!(
        "decompressed image exceeds {} GiB limit; refusing to unpack",
        MAX_UNPACKED_IMAGE_BYTES / GIB
    )
}

enum EntryKind {
    /// `.wh..wh..opq` - clear all existing contents of the parent dir.
    OpaqueWhiteout(PathBuf),
    /// `.wh.<name>` - delete the sibling `<name>`.
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
    } else if let Some(removed) = name
        .strip_prefix(WHITEOUT_PREFIX)
        // A marker with no real name would target the parent.
        .filter(|removed| !matches!(*removed, "" | "." | ".."))
    {
        EntryKind::Whiteout(parent.join(removed))
    } else {
        EntryKind::Normal
    }
}

/// Delete the rootfs path named by a whiteout marker. Missing targets are
/// not an error: a layer may whiteout something a sibling layer never
/// created, which OCI treats as a no-op.
fn apply_whiteout(dest: &Path, target: &Path) -> Result<()> {
    let Some(full) = resolve_in_root(dest, target, false) else {
        return Ok(());
    };
    remove_path(&full)
}

/// Clear the contents of a directory named by an opaque whiteout, keeping
/// the directory itself.
fn apply_opaque_whiteout(dest: &Path, dir: &Path) -> Result<()> {
    let Some(full) = resolve_in_root(dest, dir, true) else {
        return Ok(());
    };
    if !std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir()) {
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

/// Symlinks followed while resolving one path, like the kernel's ELOOP cap.
const MAX_SYMLINK_HOPS: u32 = 40;
const PARENT_DIR: &str = "..";

/// Resolve `rel` inside `root` the way a chroot would: a symlink met on the
/// way is followed, an absolute target restarts at `root` and a `..` stops at
/// `root`, so the result never leaves it. The last component is kept as is
/// unless `follow_last`, so a whiteout removes a symlink, not what it points
/// to. `None` when a component is missing, is not a directory, or the
/// symlinks loop.
fn resolve_in_root(root: &Path, rel: &Path, follow_last: bool) -> Option<PathBuf> {
    safe_join(root, rel)?;
    let mut pending: VecDeque<OsString> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    let last = if follow_last {
        None
    } else {
        pending.pop_back()
    };
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    while let Some(name) = pending.pop_front() {
        if name == PARENT_DIR {
            resolved.pop();
            continue;
        }
        let candidate = root.join(&resolved).join(&name);
        let meta = std::fs::symlink_metadata(&candidate).ok()?;
        if meta.is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                return None;
            }
            let target = std::fs::read_link(&candidate).ok()?;
            if target.is_absolute() {
                resolved.clear();
            }
            for comp in target.components().rev() {
                match comp {
                    Component::Normal(part) => pending.push_front(part.to_os_string()),
                    Component::ParentDir => pending.push_front(OsString::from(PARENT_DIR)),
                    _ => {}
                }
            }
        } else if meta.is_dir() {
            resolved.push(name);
        } else {
            return None;
        }
    }
    let mut full = root.join(resolved);
    full.extend(last);
    Some(full)
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
            // rejected outright rather than normalised - any of them is a
            // sign of a hostile or malformed layer.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Cache helpers
// ---------------------------------------------------------------------------

/// Convert an OCI digest (`sha256:abc`) to a safe filesystem directory name
/// (`sha256-abc`). Colons are not valid in directory names on some systems.
fn digest_to_dir_name(digest: &str) -> String {
    digest.replace(':', "-")
}

/// Compute the total size of all regular files under `dir` (non-recursive
/// size of symlinks is included as the symlink size). Used to populate
/// `CacheMeta::size_bytes`.
fn dir_size_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

/// A fresh image id for a commit, so it never lands on an existing entry.
fn commit_digest() -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(uuid::Uuid::new_v4().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{DIGEST_ALGORITHM_PREFIX}{hex}")
}

/// Sets its flag when dropped, so a commit whose caller went away stops.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The `meta.json` of a cache entry, or `None` when missing or unreadable.
fn read_meta(entry_dir: &Path) -> Option<CacheMeta> {
    let bytes = std::fs::read(entry_dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomically write `meta` to `path` via a sibling temp file.
fn write_meta_json(path: &Path, meta: &CacheMeta) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(meta).map_err(|e| BackendError::Internal(e.to_string()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, &bytes).map_err(BackendError::Io)?;
    std::fs::rename(&tmp, path).map_err(BackendError::Io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Materialization (independent copy)
// ---------------------------------------------------------------------------

/// Recursively materialize `src_dir` into `dst_dir` by copying each regular
/// file. Symlinks are reproduced as symlinks.
/// Directory structure is replicated via `create_dir_all`.
///
/// This is intentionally a sync fn because it is called from
/// `spawn_blocking`; avoid blocking executor threads in async callers.
pub(crate) fn materialize_dir(src_dir: &Path, dst_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![(src_dir.to_path_buf(), dst_dir.to_path_buf())];
    // Directory modes are applied last: a read-only image directory would
    // otherwise reject its own children.
    let mut dir_modes = vec![(
        dst_dir.to_path_buf(),
        std::fs::Permissions::from_mode(ROOTFS_ROOT_MODE),
    )];
    while let Some((src, dst)) = stack.pop() {
        std::fs::create_dir_all(&dst)?;
        for entry in std::fs::read_dir(&src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let meta = entry.metadata()?;
            if meta.is_dir() {
                dir_modes.push((dst_path.clone(), meta.permissions()));
                stack.push((src_path, dst_path));
            } else if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&src_path)?;
                // Best-effort: ignore if the symlink already exists.
                let _ = std::os::unix::fs::symlink(&target, &dst_path);
            } else {
                materialize_file(&src_path, &dst_path)?;
            }
        }
    }
    // Children before parents, so a restrictive parent mode never blocks one.
    for (dir, mode) in dir_modes.into_iter().rev() {
        std::fs::set_permissions(dir, mode)?;
    }
    Ok(())
}

/// Mode of the guest-visible root. The host user's umask must not narrow it,
/// or any other guest user cannot search `/`; host access is guarded by the
/// private sandbox directory above it.
const ROOTFS_ROOT_MODE: u32 = 0o755;

/// Mode of the per-sandbox directory that holds the guest-visible rootfs.
const SANDBOX_DIR_MODE: u32 = 0o700;

/// Copy `src` to `dst`. The guest writes to its rootfs in place, so sandboxes
/// must not share inodes with each other or with the image cache.
///
/// A cached file with no read bit (mode 0000 is common for `/etc/shadow`)
/// cannot be read by the unprivileged owner. The owner bit is added for the
/// copy and the original mode is restored on both files.
fn materialize_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::copy(src, dst) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let original = std::fs::metadata(src)?.permissions();
            let readable = std::fs::Permissions::from_mode(original.mode() | OWNER_READ_BIT);
            std::fs::set_permissions(src, readable)?;
            let copied = std::fs::copy(src, dst);
            std::fs::set_permissions(src, original.clone())?;
            copied?;
            std::fs::set_permissions(dst, original)
        }
        other => other.map(|_| ()),
    }
}

/// Create `rootfs` and its parent, the parent being owner-only so the
/// world-searchable guest root stays unreachable to other host users.
fn create_private_parent(rootfs: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = rootfs.parent() {
        let existed = parent.exists();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(SANDBOX_DIR_MODE)
            .create(parent)?;
        // A parent that already existed is not ours to re-mode.
        if !existed {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(SANDBOX_DIR_MODE))?;
        }
    }
    std::fs::create_dir_all(rootfs)
}

/// Owner read permission bit.
const OWNER_READ_BIT: u32 = 0o400;

// ---------------------------------------------------------------------------
// ImageStore
// ---------------------------------------------------------------------------

/// Manages the disk-persistent OCI image cache: pull, unpack, and serve
/// rootfs paths. Per-sandbox rootfs copies are materialised by copying from the
/// shared cache entry.
#[derive(Debug)]
pub struct ImageStore {
    cache_dir: PathBuf,
    max_cached_images: usize,
    puller: Arc<dyn ImagePuller>,
    max_pull_bytes: u64,
    max_cache_bytes: u64,
    /// Makes the room check and the move into the cache one step.
    install_lock: Mutex<()>,
    /// Sandbox id to the digest of the image it runs. Eviction spares these.
    holders: Mutex<HashMap<String, String>>,
}

/// What an install does when the image does not fit in the cache.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnFull {
    /// Refuse, leaving the cache as it is.
    Fail,
    /// Evict the least recently used images that nothing needs.
    Evict,
}

const UP_TO_DATE_STATUS: &str = "Image is up to date";
const DOWNLOADED_STATUS: &str = "Downloaded newer image";

/// Prefix of the cache directories a pull or commit is still filling.
const STAGING_PREFIX: &str = ".tmp-";

/// Remove staging directories a crashed daemon left behind.
fn sweep_staging(cache_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A pull's directory in the cache, removed on drop unless moved into place.
struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl ImageStore {
    /// Production constructor: pulls from real registries.
    pub fn new(cache_dir: PathBuf, max_cached_images: usize) -> Self {
        Self::with_puller(cache_dir, max_cached_images, Arc::new(OciPuller::default()))
    }

    /// Construct with an injected puller. Tests use this to stay offline.
    pub fn with_puller(
        cache_dir: PathBuf,
        max_cached_images: usize,
        puller: Arc<dyn ImagePuller>,
    ) -> Self {
        sweep_staging(&cache_dir);
        Self {
            cache_dir,
            max_cached_images,
            puller,
            max_pull_bytes: MAX_PULL_BYTES,
            max_cache_bytes: MAX_CACHE_BYTES,
            install_lock: Mutex::new(()),
            holders: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the pull size cap and the whole-cache size cap.
    pub fn with_limits(mut self, max_pull_bytes: u64, max_cache_bytes: u64) -> Self {
        self.max_pull_bytes = max_pull_bytes;
        self.max_cache_bytes = max_cache_bytes;
        self
    }

    /// Record that sandbox `sandbox_id` runs the cached image `reference`
    /// names, so eviction spares it until the sandbox is released. A moved
    /// tag does not unprotect it: the hold is on the digest.
    pub fn hold(&self, sandbox_id: &str, reference: &str) {
        let _guard = self.install_lock.lock_unpoisoned();
        if let Ok(Some((_, meta))) = self.find_cache_entry_by_ref(reference) {
            self.hold_digest(sandbox_id, &meta.digest);
        }
    }

    fn hold_digest(&self, sandbox_id: &str, digest: &str) {
        self.holders
            .lock_unpoisoned()
            .insert(sandbox_id.to_string(), digest.to_string());
    }

    pub fn release(&self, sandbox_id: &str) {
        self.holders.lock_unpoisoned().remove(sandbox_id);
    }

    fn is_held(&self, digest: &str) -> bool {
        self.holders
            .lock_unpoisoned()
            .values()
            .any(|held| held == digest)
    }

    /// Whether eviction must spare `meta`: a committed image, or one a
    /// sandbox runs.
    fn is_protected(&self, meta: &CacheMeta) -> bool {
        meta.committed || self.is_held(&meta.digest)
    }

    /// Return the shared cache rootfs path for `reference`, pulling and
    /// unpacking from the registry if the image is not already on disk.
    ///
    /// On cache hit the `meta.json::last_used_at` timestamp is refreshed so
    /// LRU eviction keeps recently-used entries alive.
    pub async fn ensure(&self, reference: &str) -> Result<PathBuf> {
        Ok(self.ensure_entry(reference, false, None).await?.0)
    }

    /// The runtime config of `reference`, pulling the image on a cache miss
    /// and again when the cached entry predates the config.
    pub async fn config(&self, reference: &str) -> Result<ImageConfig> {
        let (_, meta) = self.ensure_entry(reference, true, None).await?;
        Ok(meta.config.unwrap_or_default())
    }

    /// Make `reference` available as `policy` says and return the manifest
    /// digest it resolved to.
    pub async fn resolve(&self, reference: &str, policy: PullPolicy) -> Result<String> {
        match policy {
            PullPolicy::Never => self
                .find_cache_entry_by_ref(reference)?
                .map(|(_, meta)| meta.digest)
                .ok_or_else(|| {
                    BackendError::ImageNotFound(format!(
                        "Unable to find image '{reference}' locally"
                    ))
                }),
            PullPolicy::Always => self.refresh(reference).await,
            PullPolicy::Pinned if !is_pinned(reference) => {
                Err(BackendError::InvalidArgument(format!(
                    "--pull=pinned needs an image reference with a digest (name@sha256:...), got '{reference}'"
                )))
            }
            PullPolicy::Pinned | PullPolicy::Missing | PullPolicy::Unspecified => {
                Ok(self.ensure_entry(reference, false, None).await?.1.digest)
            }
        }
    }

    /// Pull `reference` again and move its tag to what the registry has now.
    /// The previous image stays in the cache under its digest.
    async fn refresh(&self, reference: &str) -> Result<String> {
        let mut session = self.session(None, OnFull::Evict)?;
        let (staging, pulled) = self.pull_into_temp(reference, &mut session).await?;
        let (_, meta) = self.install_entry(staging, reference, pulled, OnFull::Evict, None)?;
        Ok(meta.digest)
    }

    /// Pull `reference` into a fresh temp dir inside the cache. The caller
    /// moves or removes the returned dir.
    async fn pull_into_temp(
        &self,
        reference: &str,
        session: &mut PullSession,
    ) -> Result<(StagingDir, PulledImage)> {
        let staging = StagingDir(
            self.cache_dir
                .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())),
        );
        let temp_rootfs = staging.0.join("rootfs");

        tokio::fs::create_dir_all(&temp_rootfs)
            .await
            .map_err(BackendError::Io)?;

        let pulled = self
            .puller
            .pull_streaming(reference, &temp_rootfs, session)
            .await?;
        Ok((staging, pulled))
    }

    /// The cache entry for `reference` as it is after this call, pulled if
    /// absent. With `need_config`, an entry that has no config yet is
    /// refreshed from the registry, keeping its layers. `holder` is a sandbox
    /// that runs the image, held before the entry can be evicted.
    async fn ensure_entry(
        &self,
        reference: &str,
        need_config: bool,
        holder: Option<&str>,
    ) -> Result<(PathBuf, CacheMeta)> {
        // Fast path: already on disk.
        if let Some((_, meta)) = self.find_cache_entry_by_ref(reference)? {
            let config = if need_config && meta.config.is_none() {
                let mut session = self.session(None, OnFull::Evict)?;
                let (_staging, pulled) = self.pull_into_temp(reference, &mut session).await?;
                Some(pulled.config)
            } else {
                None
            };
            if let Some(entry) = self.touch_entry(reference, config, holder)? {
                return Ok(entry);
            }
        }

        // Slow path: pull into a temp dir, move to final location on success.
        let mut session = self.session(None, OnFull::Evict)?;
        let (staging, pulled) = self.pull_into_temp(reference, &mut session).await?;
        self.install_entry(staging, reference, pulled, OnFull::Evict, holder)
    }

    /// Mark the entry for `reference` as just used, fill in a missing
    /// `config` and hold it for `holder`. `None` when it is no longer cached.
    fn touch_entry(
        &self,
        reference: &str,
        config: Option<ImageConfig>,
        holder: Option<&str>,
    ) -> Result<Option<(PathBuf, CacheMeta)>> {
        let _guard = self.install_lock.lock_unpoisoned();
        let Some((entry_dir, mut meta)) = self.find_cache_entry_by_ref(reference)? else {
            return Ok(None);
        };
        if meta.config.is_none() {
            meta.config = config;
        }
        meta.last_used_at = chrono::Utc::now();
        write_meta_json(&entry_dir.join("meta.json"), &meta)?;
        if let Some(sandbox_id) = holder {
            self.hold_digest(sandbox_id, &meta.digest);
        }
        Ok(Some((entry_dir.join("rootfs"), meta)))
    }

    /// Move a pulled staging dir into the cache under its digest and record
    /// it. `reference` moves off any other image that held it. The staging
    /// dir is removed if the image does not fit.
    fn install_entry(
        &self,
        staging: StagingDir,
        reference: &str,
        pulled: PulledImage,
        on_full: OnFull,
        holder: Option<&str>,
    ) -> Result<(PathBuf, CacheMeta)> {
        let PulledImage { digest, config } = pulled;
        let size_bytes = dir_size_bytes(&staging.0.join("rootfs")).unwrap_or(0);
        let entry_dir = self.cache_dir.join(digest_to_dir_name(&digest));

        // Room is measured under the lock, against what other installs left.
        let _guard = self.install_lock.lock_unpoisoned();
        let now = chrono::Utc::now();
        // If the digest is already cached, discard our staging dir and add
        // the reference to the entry that is there.
        let mut meta = match read_meta(&entry_dir) {
            Some(existing) => existing,
            None => {
                match on_full {
                    OnFull::Fail => self.session(None, on_full)?.check_cache_room(size_bytes)?,
                    OnFull::Evict => self.evict_for(size_bytes)?,
                }
                std::fs::rename(&staging.0, &entry_dir).map_err(BackendError::Io)?;
                CacheMeta {
                    image_ref: digest.clone(),
                    digest,
                    size_bytes,
                    last_used_at: now,
                    layer_count: 0,
                    config: None,
                    created_at: Some(now),
                    author: None,
                    comment: None,
                    committed: false,
                    aliases: Vec::new(),
                }
            }
        };
        self.untag_others(reference, &meta.digest)?;
        meta.add_ref(reference);
        meta.config = Some(config);
        meta.last_used_at = now;
        write_meta_json(&entry_dir.join("meta.json"), &meta)?;
        if let Some(sandbox_id) = holder {
            self.hold_digest(sandbox_id, &meta.digest);
        }

        // Evict LRU entries if the cache count now exceeds the configured bound.
        self.run_lru_eviction(&meta.digest)?;

        Ok((entry_dir.join("rootfs"), meta))
    }

    /// Take `reference` off every image except the one with `digest`, as a
    /// tag moves. An image left with no reference stays under its digest.
    fn untag_others(&self, reference: &str, digest: &str) -> Result<()> {
        for (dir, meta) in self.scan_cache_entries()? {
            if meta.digest != digest && meta.has_ref(reference) {
                write_meta_json(&dir.join("meta.json"), &meta.without_ref(reference))?;
            }
        }
        Ok(())
    }

    /// Evict least recently used images that nothing needs until `needed`
    /// more bytes fit in the cache. Call with the install lock held.
    fn evict_for(&self, needed: u64) -> Result<()> {
        let mut entries = self.scan_cache_entries()?;
        let mut used: u64 = entries.iter().map(|(_, meta)| meta.size_bytes).sum();
        entries.retain(|(_, meta)| !self.is_protected(meta));
        entries.sort_by_key(|(_, meta)| meta.last_used_at);
        for (dir, meta) in entries {
            if used.saturating_add(needed) <= self.max_cache_bytes {
                break;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => used = used.saturating_sub(meta.size_bytes),
                Err(e) => tracing::warn!(
                    path = %dir.display(),
                    error = %e,
                    "failed to remove cache entry while making room"
                ),
            }
        }
        if used.saturating_add(needed) > self.max_cache_bytes {
            return Err(BackendError::ResourceExhausted(format!(
                "image cache is full: {needed} bytes do not fit in the {} bytes left",
                self.max_cache_bytes.saturating_sub(used)
            )));
        }
        Ok(())
    }

    /// Pull `reference` into the cache like `docker pull`, reporting
    /// progress lines to `progress`. A cached tag is checked against the
    /// registry and pulled again only if it moved. Nothing is cached if the
    /// pull fails, and no cached image is evicted to make room.
    pub async fn pull(
        &self,
        reference: &str,
        progress: Option<UnboundedSender<PullProgress>>,
    ) -> Result<CachedImage> {
        let parsed = validate_reference(reference)?;
        let mut session = self.session(progress, OnFull::Fail)?;
        session.report(
            parsed.tag().unwrap_or_default(),
            &format!("Pulling from {}", parsed.repository()),
        );
        let cached = self.find_cache_entry_by_ref(reference)?;
        let latest = match &cached {
            Some(_) if !is_pinned(reference) => self.puller.resolve_digest(reference).await?,
            _ => None,
        };
        let current = cached.filter(|(_, meta)| {
            is_pinned(reference) || latest.as_deref() == Some(meta.digest.as_str())
        });
        let touched = match current {
            Some(_) => self.touch_entry(reference, None, None)?,
            None => None,
        };
        let (meta, status) = match touched {
            Some((_, meta)) => (meta, UP_TO_DATE_STATUS),
            None => {
                let previous = self.find_cache_entry_by_ref(reference)?;
                let (staging, pulled) = self.pull_into_temp(reference, &mut session).await?;
                let unchanged = previous.is_some_and(|(_, old)| old.digest == pulled.digest);
                let (_, meta) =
                    self.install_entry(staging, reference, pulled, OnFull::Fail, None)?;
                (
                    meta,
                    if unchanged {
                        UP_TO_DATE_STATUS
                    } else {
                        DOWNLOADED_STATUS
                    },
                )
            }
        };
        session.report("", &format!("Digest: {}", meta.digest));
        session.report("", &format!("Status: {status} for {reference}"));
        Ok(self.cached_image(&meta, reference))
    }

    /// Commit the rootfs of a sandbox as a new image, like `docker commit`.
    /// `base` is the config of the sandbox, with `spec.changes` laid over it.
    /// A tag already in use moves to the new image and the old one stays by
    /// digest. The cache is untouched if the copy fails, is cancelled or does
    /// not fit, and nothing is evicted to make room. A committed image is
    /// never evicted.
    pub async fn commit(
        self: &Arc<Self>,
        rootfs: &Path,
        base: ImageConfig,
        spec: &CommitSpec,
        skip: &'static [&'static str],
    ) -> Result<CachedImage> {
        if let Some(reference) = &spec.reference {
            validate_reference(reference)?;
        }
        let mut config = base;
        commit::apply_changes(&mut config, &spec.changes)?;

        let cancel = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let (store, rootfs, spec) = (self.clone(), rootfs.to_path_buf(), spec.clone());
        tokio::task::spawn_blocking(move || {
            store.commit_blocking(&rootfs, config, &spec, skip, &cancel)
        })
        .await
        .map_err(|e| BackendError::Internal(format!("commit task: {e}")))?
    }

    /// The whole commit after validation. It owns the staging directory, so
    /// the directory is removed only once the copy has stopped.
    fn commit_blocking(
        &self,
        rootfs: &Path,
        config: ImageConfig,
        spec: &CommitSpec,
        skip: &[&str],
        cancel: &AtomicBool,
    ) -> Result<CachedImage> {
        let room = self.session(None, OnFull::Fail)?.cache_room;
        let limits = CopyLimits {
            max_bytes: room.min(commit::MAX_COMMIT_BYTES),
            ..CopyLimits::default()
        };
        let staging = StagingDir(
            self.cache_dir
                .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4())),
        );
        let staged_rootfs = staging.0.join("rootfs");
        std::fs::create_dir_all(&staged_rootfs).map_err(BackendError::Io)?;
        let size_bytes = commit::copy_tree(rootfs, &staged_rootfs, skip, limits, cancel).map_err(
            |e| match e {
                CopyError::TooLarge if room <= commit::MAX_COMMIT_BYTES => {
                    BackendError::ResourceExhausted(format!(
                        "image cache is full: the filesystem does not fit in the {room} bytes left"
                    ))
                }
                CopyError::TooLarge => BackendError::ResourceExhausted(format!(
                    "the filesystem is larger than the commit limit of {} bytes",
                    commit::MAX_COMMIT_BYTES
                )),
                CopyError::Cancelled => BackendError::Internal("commit cancelled".to_string()),
                CopyError::Backend(e) => e,
            },
        )?;

        let now = chrono::Utc::now();
        let digest = commit_digest();
        let mut config = config;
        config.history.push(HistoryEntry {
            created: Some(now),
            created_by: String::new(),
            comment: spec.message.clone(),
            empty_layer: false,
            size_bytes,
        });
        let meta = CacheMeta {
            image_ref: spec.reference.clone().unwrap_or_else(|| digest.clone()),
            digest,
            size_bytes,
            last_used_at: now,
            layer_count: 0,
            config: Some(config),
            created_at: Some(now),
            author: Some(spec.author.clone()).filter(|a| !a.is_empty()),
            comment: Some(spec.message.clone()).filter(|m| !m.is_empty()),
            committed: true,
            aliases: Vec::new(),
        };
        write_meta_json(&staging.0.join("meta.json"), &meta)?;

        // The room and the tag owner are read again under the lock: another
        // commit or pull may have changed either since the first look.
        let _guard = self.install_lock.lock_unpoisoned();
        if cancel.load(Ordering::Relaxed) {
            return Err(BackendError::Internal("commit cancelled".to_string()));
        }
        self.session(None, OnFull::Fail)?
            .check_cache_room(size_bytes)?;
        let previous = match &spec.reference {
            Some(reference) => self.find_cache_entry_by_ref(reference)?,
            None => None,
        };
        // The old image keeps its place under its digest; the tag moves.
        if let (Some((dir, old)), Some(reference)) = (&previous, &spec.reference) {
            write_meta_json(&dir.join("meta.json"), &old.without_ref(reference))?;
        }
        let entry_dir = self.cache_dir.join(digest_to_dir_name(&meta.digest));
        if let Err(e) = std::fs::rename(&staging.0, &entry_dir) {
            if let Some((dir, old)) = &previous {
                let _ = write_meta_json(&dir.join("meta.json"), old);
            }
            return Err(BackendError::Io(e));
        }
        Ok(self.cached_image(&meta, &meta.image_ref))
    }

    /// A pull session limited by the pull cap and the room left in the
    /// cache. With `OnFull::Evict`, the room counts what eviction can free.
    fn session(
        &self,
        progress: Option<UnboundedSender<PullProgress>>,
        on_full: OnFull,
    ) -> Result<PullSession> {
        let used: u64 = self
            .scan_cache_entries()?
            .iter()
            .filter(|(_, meta)| on_full == OnFull::Fail || self.is_protected(meta))
            .map(|(_, meta)| meta.size_bytes)
            .sum();
        Ok(PullSession::new(
            self.max_pull_bytes,
            self.max_cache_bytes.saturating_sub(used),
            progress,
        ))
    }

    /// `meta` seen as the image named `reference`.
    fn cached_image(&self, meta: &CacheMeta, reference: &str) -> CachedImage {
        let entry_dir = self.cache_dir.join(digest_to_dir_name(&meta.digest));
        CachedImage {
            reference: reference.to_string(),
            rootfs_path: entry_dir.join("rootfs"),
            digest: meta.digest.clone(),
            pulled_at: meta.created_at.unwrap_or(meta.last_used_at).into(),
            size_bytes: meta.size_bytes,
        }
    }

    /// Materialize the image rootfs for `reference` into `dest` (the
    /// per-sandbox rootfs directory). On cache miss the image is pulled first.
    ///
    /// Each file is copied from the cache entry, so the sandbox copy is
    /// independent: neither guest writes nor cache eviction affect the other.
    ///
    /// The image is held for `sandbox_id` before it can be evicted; the
    /// caller releases it if the sandbox is never created.
    ///
    /// This is the entry point called by the krunvm backend.
    pub async fn prepare_sandbox_rootfs(
        &self,
        sandbox_id: &str,
        reference: &str,
        dest: &Path,
    ) -> Result<()> {
        let (cache_rootfs, _) = self
            .ensure_entry(reference, false, Some(sandbox_id))
            .await?;
        let dest_owned = dest.to_path_buf();
        tokio::task::spawn_blocking(move || create_private_parent(&dest_owned))
            .await
            .map_err(|e| BackendError::Internal(format!("rootfs dir task: {e}")))?
            .map_err(BackendError::Io)?;
        let src = cache_rootfs.clone();
        let dst = dest.to_path_buf();
        tokio::task::spawn_blocking(move || materialize_dir(&src, &dst))
            .await
            .map_err(|e| BackendError::Internal(format!("materialize task: {e}")))?
            .map_err(BackendError::Io)?;
        Ok(())
    }

    /// Check whether an image reference is present in the on-disk cache.
    pub async fn is_cached(&self, reference: &str) -> bool {
        self.find_cache_entry_by_ref(reference)
            .ok()
            .flatten()
            .is_some()
    }

    /// The cached rootfs of `reference`, or `None` when it is not cached.
    /// Unlike `ensure`, this never pulls.
    pub async fn cached_rootfs(&self, reference: &str) -> Result<Option<PathBuf>> {
        Ok(self
            .find_cache_entry_by_ref(reference)?
            .map(|(entry_dir, _)| entry_dir.join("rootfs")))
    }

    /// Remove `reference` from the cache like `docker rmi`: an image that
    /// other references still name loses only this one, otherwise its
    /// directory is deleted. Removing by digest deletes the image. An image a
    /// sandbox runs is never deleted. Per-sandbox rootfs copies are unaffected.
    pub async fn remove(&self, reference: &str) -> Result<()> {
        let _guard = self.install_lock.lock_unpoisoned();
        let (entry_dir, meta) = self
            .find_cache_entry_by_ref(reference)?
            .ok_or_else(|| BackendError::ImageNotFound(format!("No such image: {reference}")))?;
        let rest = meta.without_ref(reference);
        if reference != meta.digest && !rest.is_untagged() {
            return write_meta_json(&entry_dir.join("meta.json"), &rest);
        }
        if self.is_held(&meta.digest) {
            return Err(BackendError::InvalidArgument(format!(
                "conflict: unable to remove {reference}, a container is using its image"
            )));
        }
        std::fs::remove_dir_all(&entry_dir).map_err(BackendError::Io)
    }

    /// List all cached images read from disk, one row per reference.
    pub async fn list(&self) -> Vec<CachedImage> {
        self.scan_cache_entries()
            .unwrap_or_default()
            .iter()
            .flat_map(|(_, meta)| {
                meta.refs()
                    .map(|reference| self.cached_image(meta, reference))
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Read all valid cache entries from `cache_dir`.
    fn scan_cache_entries(&self) -> Result<Vec<(PathBuf, CacheMeta)>> {
        let read_dir = match std::fs::read_dir(&self.cache_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(BackendError::Io(e)),
        };

        let mut entries = Vec::new();
        for item in read_dir {
            let item = item.map_err(BackendError::Io)?;
            let entry_dir = item.path();
            if !entry_dir.is_dir() {
                continue;
            }
            // Skip temp dirs from in-progress pulls.
            let dir_name = entry_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir_name.starts_with(STAGING_PREFIX) {
                continue;
            }
            if let Some(meta) = read_meta(&entry_dir) {
                entries.push((entry_dir, meta));
            }
        }
        // Directory order is arbitrary; a stable one keeps lookups repeatable.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Find the first cache entry whose `image_ref` matches `reference`.
    fn find_cache_entry_by_ref(&self, reference: &str) -> Result<Option<(PathBuf, CacheMeta)>> {
        Ok(self
            .scan_cache_entries()?
            .into_iter()
            .find(|(_, meta)| meta.has_ref(reference)))
    }

    /// Evict the least-recently-used cache entries until the count is within
    /// `max_cached_images`, sparing committed and held images and
    /// `keep_digest`, the image just installed.
    fn run_lru_eviction(&self, keep_digest: &str) -> Result<()> {
        let mut entries = self.scan_cache_entries()?;
        if self.max_cached_images == 0 || entries.len() <= self.max_cached_images {
            return Ok(());
        }
        // Oldest first (ascending last_used_at).
        let to_evict = entries.len() - self.max_cached_images;
        entries.retain(|(_, meta)| !self.is_protected(meta) && meta.digest != keep_digest);
        entries.sort_by_key(|(_, meta)| meta.last_used_at);
        for (entry_dir, _) in entries.into_iter().take(to_evict) {
            // Best-effort: a failed eviction must not fail the pull that
            // triggered it, but a silent failure lets the cache grow past
            // max_cached_images with no way to notice, so log and count it.
            if let Err(e) = std::fs::remove_dir_all(&entry_dir) {
                tracing::warn!(
                    path = %entry_dir.display(),
                    error = %e,
                    "failed to remove cache entry during LRU eviction"
                );
                metrics::counter!("wardd_image_cache_eviction_failure_total").increment(1);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    fn index_entry(os: &str, arch: &str, digest: &str) -> oci_client::manifest::ImageIndexEntry {
        serde_json::from_value(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest,
            "size": 1,
            "platform": { "os": os, "architecture": arch },
        }))
        .expect("index entry")
    }

    #[test]
    fn given_mixed_platforms_when_resolving_then_linux_for_host_arch_is_chosen() {
        let host_arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            other => other,
        };
        let other_arch = if host_arch == "arm64" {
            "amd64"
        } else {
            "arm64"
        };
        let entries = vec![
            index_entry("windows", host_arch, "sha256:windows"),
            index_entry("linux", other_arch, "sha256:wrong-arch"),
            index_entry("linux", host_arch, "sha256:right"),
        ];

        assert_eq!(
            linux_host_arch_resolver(&entries).as_deref(),
            Some("sha256:right")
        );
    }

    #[test]
    fn given_no_linux_variant_for_host_arch_when_resolving_then_none() {
        let entries = vec![index_entry("windows", "amd64", "sha256:windows")];

        assert_eq!(linux_host_arch_resolver(&entries), None);
    }

    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use tempfile::TempDir;

    // ----- Fake puller: keeps cache/list/remove tests offline -------------

    /// Materialises a minimal rootfs (a `bin/` dir) without touching the
    /// network, so the `ImageStore` bookkeeping can be tested hermetically.
    /// Returns a digest derived from the image reference so that different
    /// references get distinct cache entries (as they would in production).
    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl ImagePuller for FakePuller {
        async fn pull(&self, reference: &str, dest: &Path) -> Result<PulledImage> {
            std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
            // Deterministic but reference-specific digest so each distinct
            // image tag gets its own cache entry directory.
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(PulledImage {
                digest: format!("sha256:{hash:016x}"),
                config: fake_config(),
            })
        }
    }

    fn fake_config() -> ImageConfig {
        ImageConfig {
            entrypoint: vec!["/entry".to_string()],
            cmd: vec!["/bin/sh".to_string()],
            working_dir: "/app".to_string(),
            user: "nobody".to_string(),
            env: vec!["PATH=/bin".to_string()],
            ..ImageConfig::default()
        }
    }

    fn store_in_tempdir() -> (ImageStore, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, Arc::new(FakePuller));
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

    fn unpack_layer(data: &[u8], media_type: &str, dest: &Path) -> Result<()> {
        unpack_into(data, media_type, dest, MAX_UNPACKED_IMAGE_BYTES)
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

    // ----- disk cache: miss then hit -------------------------------------

    #[tokio::test]
    async fn given_cache_miss_when_ensure_then_meta_json_written() {
        let (store, tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("ensure");

        // At least one meta.json must exist under cache_dir.
        let metas: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read cache_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("meta.json"))
            .filter(|p| p.exists())
            .collect();
        assert!(!metas.is_empty(), "expected at least one meta.json");

        let bytes = std::fs::read(&metas[0]).expect("read meta.json");
        let meta: CacheMeta = serde_json::from_slice(&bytes).expect("parse meta.json");
        assert_eq!(meta.image_ref, "alpine:latest");
        assert!(meta.digest.starts_with("sha256:"), "got: {}", meta.digest);
    }

    #[tokio::test]
    async fn given_cache_hit_when_ensure_second_time_then_puller_not_called_again() {
        /// Puller that panics after the first call to ensure the cache hit
        /// path never reaches the registry.
        #[derive(Debug)]
        struct OnceOnlyPuller {
            called: std::sync::Mutex<bool>,
        }

        #[async_trait::async_trait]
        impl ImagePuller for OnceOnlyPuller {
            async fn pull(&self, _reference: &str, dest: &Path) -> Result<PulledImage> {
                let mut guard = self.called.lock().unwrap();
                assert!(
                    !*guard,
                    "puller called more than once (cache miss on second call)"
                );
                *guard = true;
                std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
                Ok(PulledImage::without_config("sha256:onlyonce".to_string()))
            }
        }

        let tmp = TempDir::new().expect("tempdir");
        let puller = Arc::new(OnceOnlyPuller {
            called: std::sync::Mutex::new(false),
        });
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, puller);

        // First call: cache miss - puller fires.
        store.ensure("alpine:latest").await.expect("first ensure");
        // Second call: cache hit - puller must NOT fire (else OnceOnlyPuller panics).
        store.ensure("alpine:latest").await.expect("second ensure");
    }

    // ----- image config ---------------------------------------------------

    #[tokio::test]
    async fn given_pulled_image_when_config_then_returns_the_pulled_config() {
        let (store, _tmp) = store_in_tempdir();

        let config = store.config("alpine:latest").await.expect("config");

        assert_eq!(config, fake_config());
    }

    #[tokio::test]
    async fn given_cached_image_when_config_again_then_config_survives_the_cache_hit() {
        let (store, _tmp) = store_in_tempdir();
        store.ensure("alpine:latest").await.expect("ensure");

        let config = store.config("alpine:latest").await.expect("config");

        assert_eq!(config, fake_config());
    }

    const OLD_FORMAT_META: &str = r#"{"image_ref":"alpine:latest","digest":"sha256:old",
        "size_bytes":1,"last_used_at":"2026-01-01T00:00:00Z","layer_count":0}"#;

    #[test]
    fn given_meta_json_without_config_when_parsed_then_config_is_unknown() {
        let meta: CacheMeta = serde_json::from_str(OLD_FORMAT_META).expect("parse old meta");

        assert_eq!(meta.config, None);
    }

    /// Writes a cache entry the way a release without image configs did.
    fn seed_old_format_entry(cache_dir: &Path) -> PathBuf {
        let entry_dir = cache_dir.join("sha256-old");
        std::fs::create_dir_all(entry_dir.join("rootfs/bin")).expect("create rootfs");
        std::fs::write(entry_dir.join("rootfs/kept"), b"layer").expect("write layer file");
        std::fs::write(entry_dir.join("meta.json"), OLD_FORMAT_META).expect("write meta");
        entry_dir
    }

    #[tokio::test]
    async fn given_old_format_cache_entry_when_config_then_image_is_pulled_to_fill_it() {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let entry_dir = seed_old_format_entry(tmp.path());
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, Arc::new(FakePuller));

        // Act
        let config = store.config("alpine:latest").await.expect("config");

        // Assert
        assert_eq!(config, fake_config());
        assert!(entry_dir.join("rootfs/kept").exists(), "layers are kept");
        let again = store.config("alpine:latest").await.expect("config again");
        assert_eq!(again, fake_config());
    }

    #[tokio::test]
    async fn given_old_format_cache_entry_when_ensure_then_no_pull_happens() {
        // Arrange
        #[derive(Debug)]
        struct NeverPuller;

        #[async_trait::async_trait]
        impl ImagePuller for NeverPuller {
            async fn pull(&self, _reference: &str, _dest: &Path) -> Result<PulledImage> {
                panic!("a request that needs no config must not pull");
            }
        }
        let tmp = TempDir::new().expect("tempdir");
        seed_old_format_entry(tmp.path());
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, Arc::new(NeverPuller));

        // Act
        let rootfs = store.ensure("alpine:latest").await.expect("ensure");

        // Assert
        assert!(rootfs.join("kept").exists());
    }

    #[test]
    fn given_oci_config_blob_when_parsed_then_runtime_fields_are_read() {
        let blob = br#"{"architecture":"arm64","config":{"Cmd":["/bin/sh"],
            "Entrypoint":null,"WorkingDir":"/w","User":"u","Env":["A=1"]}}"#;

        let config = ImageConfig::from_blob(blob).expect("parse blob");

        assert_eq!(
            config,
            ImageConfig {
                entrypoint: vec![],
                cmd: vec!["/bin/sh".to_string()],
                working_dir: "/w".to_string(),
                user: "u".to_string(),
                env: vec!["A=1".to_string()],
                ..ImageConfig::default()
            }
        );
    }

    // ----- pull and list --------------------------------------------------

    /// A registry stand-in that records its calls and "downloads" layers of
    /// the given byte size.
    #[derive(Debug)]
    struct RecordingPuller {
        layer_bytes: u64,
        calls: std::sync::atomic::AtomicUsize,
        /// Digests the "registry" serves for a reference, when not derived.
        digests: Mutex<HashMap<String, String>>,
    }

    impl RecordingPuller {
        fn new(layer_bytes: u64) -> Arc<Self> {
            Arc::new(Self {
                layer_bytes,
                calls: std::sync::atomic::AtomicUsize::new(0),
                digests: Mutex::new(HashMap::new()),
            })
        }

        /// Make the registry serve `digest` for `reference` from now on.
        fn point(&self, reference: &str, digest: &str) {
            self.digests
                .lock()
                .expect("digests")
                .insert(reference.to_string(), digest.to_string());
        }

        fn digest_of(&self, reference: &str) -> String {
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            self.digests
                .lock()
                .expect("digests")
                .get(reference)
                .cloned()
                .unwrap_or_else(|| format!("sha256:{hash:016x}"))
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ImagePuller for RecordingPuller {
        async fn pull(&self, reference: &str, dest: &Path) -> Result<PulledImage> {
            self.pull_streaming(
                reference,
                dest,
                &mut PullSession::new(u64::MAX, u64::MAX, None),
            )
            .await
        }

        async fn pull_streaming(
            &self,
            reference: &str,
            dest: &Path,
            session: &mut PullSession,
        ) -> Result<PulledImage> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            session.charge(self.layer_bytes)?;
            std::fs::write(dest.join("layer"), vec![0u8; self.layer_bytes as usize])
                .map_err(BackendError::Io)?;
            Ok(PulledImage::without_config(self.digest_of(reference)))
        }

        async fn resolve_digest(&self, reference: &str) -> Result<Option<String>> {
            Ok(Some(self.digest_of(reference)))
        }
    }

    fn limited_store(
        puller: Arc<RecordingPuller>,
        max_pull: u64,
        max_cache: u64,
    ) -> (ImageStore, TempDir) {
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, puller)
            .with_limits(max_pull, max_cache);
        (store, tmp)
    }

    fn cache_dir_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read cache dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    }

    const SMALL_CAP: u64 = 100;

    #[tokio::test]
    async fn given_layers_over_the_pull_cap_when_pulled_then_it_aborts_and_caches_nothing() {
        // Arrange
        let (store, tmp) = limited_store(RecordingPuller::new(SMALL_CAP + 1), SMALL_CAP, u64::MAX);

        // Act
        let error = store.pull("alpine:3", None).await.expect_err("too big");

        // Assert
        assert!(error.to_string().contains("pull limit"), "{error}");
        assert!(cache_dir_names(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn given_a_cache_that_would_pass_its_cap_when_pulled_then_it_is_full_and_unchanged() {
        // Arrange
        let (store, tmp) = limited_store(RecordingPuller::new(SMALL_CAP), u64::MAX, SMALL_CAP + 50);
        store.pull("first", None).await.expect("first fits");
        let before = cache_dir_names(tmp.path());

        // Act
        let error = store.pull("second", None).await.expect_err("cache full");

        // Assert
        assert!(error.to_string().contains("cache is full"), "{error}");
        assert_eq!(cache_dir_names(tmp.path()), before);
        assert_eq!(store.list().await.len(), 1);
    }

    #[rstest::rstest]
    #[case::uppercase("Alpine")]
    #[case::spaces("not a ref")]
    #[case::traversal("../../etc")]
    #[tokio::test]
    async fn given_an_invalid_reference_when_pulled_then_the_registry_is_never_called(
        #[case] reference: &str,
    ) {
        // Arrange
        let puller = RecordingPuller::new(1);
        let (store, _tmp) = limited_store(puller.clone(), SMALL_CAP, SMALL_CAP);

        // Act
        let error = store.pull(reference, None).await.expect_err("invalid");

        // Assert
        assert!(error.to_string().contains("invalid reference"), "{error}");
        assert_eq!(puller.calls(), 0);
    }

    #[tokio::test]
    async fn given_a_pulled_image_when_listed_then_it_has_a_size_above_zero() {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(10), SMALL_CAP, SMALL_CAP);
        store.pull("alpine:3", None).await.expect("pull");

        // Act
        let images = store.list().await;

        // Assert
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference, "alpine:3");
        assert!(images[0].size_bytes > 0);
    }

    #[tokio::test]
    async fn given_a_cached_image_when_pulled_again_then_it_is_up_to_date_without_a_registry_call()
    {
        // Arrange
        let puller = RecordingPuller::new(10);
        let (store, _tmp) = limited_store(puller.clone(), SMALL_CAP, SMALL_CAP);
        store.pull("alpine:3", None).await.expect("first");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Act
        store.pull("alpine:3", Some(tx)).await.expect("second");

        // Assert
        let mut last = String::new();
        while let Some(line) = rx.recv().await {
            last = line.status;
        }
        assert_eq!(last, "Status: Image is up to date for alpine:3");
        assert_eq!(puller.calls(), 1);
    }

    // ----- several references on one image --------------------------------

    #[tokio::test]
    async fn given_two_tags_on_one_digest_when_pulled_then_both_list_and_resolve_from_one_entry() {
        // Arrange
        let puller = RecordingPuller::new(10);
        puller.point("alpine:latest", "sha256:aaaa");
        puller.point("alpine:3.20", "sha256:aaaa");
        let (store, tmp) = limited_store(puller.clone(), SMALL_CAP, SMALL_CAP * 10);

        // Act
        store.pull("alpine:latest", None).await.expect("latest");
        store.pull("alpine:3.20", None).await.expect("3.20");

        // Assert
        let mut references: Vec<_> = store
            .list()
            .await
            .into_iter()
            .map(|i| i.reference)
            .collect();
        references.sort();
        assert_eq!(references, vec!["alpine:3.20", "alpine:latest"]);
        assert!(store.is_cached("alpine:latest").await);
        assert!(store.is_cached("alpine:3.20").await);
        assert_eq!(cache_dir_names(tmp.path()).len(), 1);
    }

    #[tokio::test]
    async fn given_an_image_with_two_tags_when_one_is_removed_then_the_other_stays() {
        // Arrange
        let puller = RecordingPuller::new(10);
        puller.point("alpine:latest", "sha256:aaaa");
        puller.point("alpine:3.20", "sha256:aaaa");
        let (store, _tmp) = limited_store(puller, SMALL_CAP, SMALL_CAP * 10);
        store.pull("alpine:latest", None).await.expect("latest");
        store.pull("alpine:3.20", None).await.expect("3.20");

        // Act
        store.remove("alpine:latest").await.expect("untag");

        // Assert
        assert!(!store.is_cached("alpine:latest").await);
        assert!(store.is_cached("alpine:3.20").await);
    }

    #[test]
    fn given_meta_json_without_aliases_when_parsed_then_it_has_none() {
        // Arrange
        let json = r#"{"image_ref":"a:1","digest":"sha256:aa","size_bytes":1,
            "last_used_at":"2026-01-01T00:00:00Z","layer_count":0}"#;

        // Act
        let meta: CacheMeta = serde_json::from_str(json).expect("old metadata");

        // Assert
        assert!(meta.aliases.is_empty());
        assert!(meta.has_ref("a:1"));
    }

    // ----- re-resolving a tag ---------------------------------------------

    #[tokio::test]
    async fn given_a_tag_that_moved_when_pulled_then_it_is_pulled_and_the_old_image_stays_by_digest()
     {
        // Arrange
        let puller = RecordingPuller::new(10);
        let (store, _tmp) = limited_store(puller.clone(), SMALL_CAP, SMALL_CAP * 10);
        let first = store.pull("alpine:3", None).await.expect("first");
        puller.point("alpine:3", "sha256:bbbb");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Act
        let second = store.pull("alpine:3", Some(tx)).await.expect("second");

        // Assert
        let mut last = String::new();
        while let Some(line) = rx.recv().await {
            last = line.status;
        }
        assert_eq!(last, "Status: Downloaded newer image for alpine:3");
        assert_eq!(second.digest, "sha256:bbbb");
        assert_eq!(puller.calls(), 2);
        let listed: HashMap<_, _> = store
            .list()
            .await
            .into_iter()
            .map(|i| (i.digest, i.reference))
            .collect();
        assert_eq!(listed[&second.digest], "alpine:3");
        assert_eq!(listed[&first.digest], first.digest);
    }

    // ----- eviction by bytes on create ------------------------------------

    #[tokio::test]
    async fn given_a_full_cache_when_a_sandbox_image_is_ensured_then_the_least_recently_used_is_evicted()
     {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(100), u64::MAX, 250);
        store.ensure("a").await.expect("a");
        store.ensure("b").await.expect("b");

        // Act
        store.ensure("c").await.expect("c makes room");

        // Assert
        assert!(!store.is_cached("a").await);
        assert!(store.is_cached("b").await);
        assert!(store.is_cached("c").await);
    }

    #[tokio::test]
    async fn given_a_full_cache_when_the_oldest_image_is_held_by_a_sandbox_then_it_is_spared() {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(100), u64::MAX, 250);
        store.ensure("a").await.expect("a");
        store.ensure("b").await.expect("b");
        store.hold("sandbox-1", "a");

        // Act
        store.ensure("c").await.expect("c makes room");

        // Assert
        assert!(store.is_cached("a").await);
        assert!(!store.is_cached("b").await);
    }

    #[tokio::test]
    async fn given_every_image_held_when_a_sandbox_image_is_ensured_then_the_cache_is_full() {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(100), u64::MAX, 250);
        store.ensure("a").await.expect("a");
        store.ensure("b").await.expect("b");
        store.hold("sandbox-1", "a");
        store.hold("sandbox-2", "b");

        // Act
        let error = store.ensure("c").await.expect_err("nothing to evict");

        // Assert
        assert!(
            matches!(error, BackendError::ResourceExhausted(_)),
            "{error}"
        );
        assert!(store.is_cached("a").await && store.is_cached("b").await);
    }

    // ----- error mapping --------------------------------------------------

    fn server_error(code: u16) -> OciDistributionError {
        OciDistributionError::ServerError {
            code,
            url: "http://registry.test/v2/".to_string(),
            message: String::new(),
        }
    }

    fn io_error(kind: std::io::ErrorKind) -> OciDistributionError {
        OciDistributionError::IoError(std::io::Error::from(kind))
    }

    #[rstest::rstest]
    #[case::not_found(server_error(404), "manifest for alpine:x not found: manifest unknown")]
    #[case::denied(
        OciDistributionError::UnauthorizedError { url: String::new() },
        "pull access denied for alpine:x"
    )]
    fn given_a_registry_that_lacks_the_image_when_mapped_then_it_is_not_found_in_dockers_words(
        #[case] error: OciDistributionError,
        #[case] wording: &str,
    ) {
        // Act
        let mapped = pull_error("alpine:x", error);

        // Assert
        assert!(
            matches!(&mapped, BackendError::ImageNotFound(m) if m.starts_with(wording)),
            "{mapped}"
        );
    }

    #[rstest::rstest]
    #[case::reset(io_error(std::io::ErrorKind::ConnectionReset))]
    #[case::unavailable(server_error(503))]
    fn given_a_network_failure_when_mapped_then_it_is_unavailable(
        #[case] error: OciDistributionError,
    ) {
        // Act
        let mapped = pull_error("alpine:x", error);

        // Assert
        assert!(matches!(mapped, BackendError::Unavailable(_)), "{mapped}");
    }

    #[test]
    fn given_a_blob_longer_than_declared_when_mapped_then_it_is_resource_exhausted() {
        // Act
        let mapped = pull_error("alpine:x", io_error(FileTooLarge));

        // Assert
        assert!(
            matches!(mapped, BackendError::ResourceExhausted(_)),
            "{mapped}"
        );
    }

    #[test]
    fn given_a_layer_over_the_unpack_cap_when_unpacked_then_it_is_resource_exhausted() {
        // Arrange
        let dest = TempDir::new().expect("tempdir");
        let layer = gzip_layer(&[("big", &[0u8; 4096])]);

        // Act
        let error = unpack_into(layer.as_slice(), GZIP_MEDIA, dest.path(), 16).expect_err("cap");

        // Assert
        assert!(
            matches!(error, BackendError::ResourceExhausted(_)),
            "{error}"
        );
    }

    // ----- whiteouts through symlinks -------------------------------------

    fn rootfs_with_usr_bin() -> TempDir {
        let root = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("usr/bin")).expect("usr/bin");
        std::fs::write(root.path().join("usr/bin/ls"), b"x").expect("ls");
        root
    }

    #[rstest::rstest]
    #[case::relative("usr/bin")]
    #[case::absolute("/usr/bin")]
    fn given_a_symlinked_parent_inside_the_rootfs_when_whiteout_then_the_target_is_removed(
        #[case] link_target: &str,
    ) {
        // Arrange
        let root = rootfs_with_usr_bin();
        std::os::unix::fs::symlink(link_target, root.path().join("bin")).expect("symlink");

        // Act
        unpack_layer(&gzip_marker("bin/.wh.ls"), GZIP_MEDIA, root.path()).expect("unpack");

        // Assert
        assert!(!root.path().join("usr/bin/ls").exists());
        assert!(root.path().join("bin").is_symlink());
    }

    #[test]
    fn given_a_symlinked_parent_when_opaque_whiteout_then_the_directory_behind_it_is_cleared() {
        // Arrange
        let root = rootfs_with_usr_bin();
        std::os::unix::fs::symlink("usr/bin", root.path().join("bin")).expect("symlink");

        // Act
        unpack_layer(&gzip_marker("bin/.wh..wh..opq"), GZIP_MEDIA, root.path()).expect("unpack");

        // Assert
        assert!(root.path().join("usr/bin").is_dir());
        assert!(!root.path().join("usr/bin/ls").exists());
    }

    #[rstest::rstest]
    #[case::dotdot("../../outside")]
    #[case::absolute_host_path("OUTSIDE")]
    fn given_a_symlink_that_aims_outside_when_whiteout_through_it_then_nothing_outside_is_removed(
        #[case] link_target: &str,
    ) {
        // Arrange
        let sandbox = TempDir::new().expect("tempdir");
        let root = sandbox.path().join("rootfs/nested");
        let outside = sandbox.path().join("outside");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("victim"), b"x").expect("victim");
        let target = link_target.replace("OUTSIDE", outside.to_str().expect("utf8"));
        std::os::unix::fs::symlink(target, root.join("evil")).expect("symlink");

        // Act
        unpack_layer(&gzip_marker("evil/.wh.victim"), GZIP_MEDIA, &root).expect("unpack");

        // Assert
        assert!(outside.join("victim").exists());
    }

    #[test]
    fn given_a_symlink_loop_when_whiteout_through_it_then_it_is_skipped() {
        // Arrange
        let root = TempDir::new().expect("tempdir");
        std::os::unix::fs::symlink("b", root.path().join("a")).expect("a");
        std::os::unix::fs::symlink("a", root.path().join("b")).expect("b");

        // Act
        let result = unpack_layer(&gzip_marker("a/.wh.x"), GZIP_MEDIA, root.path());

        // Assert
        assert!(result.is_ok());
    }

    // ----- count eviction spares what must stay ---------------------------

    impl ImageStore {
        /// Ensure and hold `reference`; returns the digest it is cached by.
        async fn pull_for_hold(&self, reference: &str) -> String {
            self.ensure(reference).await.expect("ensure");
            self.hold("sandbox-1", reference);
            self.find_cache_entry_by_ref(reference)
                .expect("scan")
                .expect("cached")
                .1
                .digest
        }
    }

    fn mark_committed(store: &ImageStore, reference: &str) {
        let (dir, mut meta) = store
            .find_cache_entry_by_ref(reference)
            .expect("scan")
            .expect("cached");
        meta.committed = true;
        write_meta_json(&dir.join("meta.json"), &meta).expect("write meta");
    }

    #[tokio::test]
    async fn given_all_old_images_committed_when_a_new_one_is_pulled_over_the_count_then_it_is_kept()
     {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 2, Arc::new(FakePuller));
        for name in ["a", "b", "c"] {
            store.ensure(name).await.expect("seed");
            mark_committed(&store, name);
        }

        // Act
        store.ensure("new").await.expect("new image");

        // Assert
        assert!(store.is_cached("new").await);
        assert!(store.is_cached("a").await);
    }

    #[tokio::test]
    async fn given_an_old_image_held_by_a_sandbox_when_the_count_is_passed_then_it_survives() {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 2, Arc::new(FakePuller));
        store.ensure("a").await.expect("a");
        store.hold("sandbox-1", "a");
        store.ensure("b").await.expect("b");

        // Act
        store.ensure("c").await.expect("c");

        // Assert
        assert!(store.is_cached("a").await);
        assert!(!store.is_cached("b").await);
        assert!(store.is_cached("c").await);
    }

    #[tokio::test]
    async fn given_a_held_image_whose_tag_moved_when_room_is_needed_then_it_is_still_spared() {
        // Arrange
        let puller = RecordingPuller::new(100);
        let (store, _tmp) = limited_store(puller.clone(), u64::MAX, 250);
        let held = store.pull_for_hold("a").await;
        puller.point("a", "sha256:moved");
        store
            .resolve("a", PullPolicy::Always)
            .await
            .expect("move tag");

        // Act
        store.ensure("c").await.expect("c makes room");

        // Assert
        assert!(store.is_cached(&held).await);
    }

    // ----- remove ---------------------------------------------------------

    #[tokio::test]
    async fn given_a_tagged_image_when_removed_by_digest_then_the_image_is_deleted() {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(10), SMALL_CAP, SMALL_CAP * 10);
        let image = store.pull("alpine:3", None).await.expect("pull");

        // Act
        store.remove(&image.digest).await.expect("remove by digest");

        // Assert
        assert!(!store.is_cached("alpine:3").await);
    }

    #[tokio::test]
    async fn given_an_image_a_sandbox_runs_when_removed_then_it_is_refused() {
        // Arrange
        let (store, _tmp) = limited_store(RecordingPuller::new(10), SMALL_CAP, SMALL_CAP * 10);
        store.pull("alpine:3", None).await.expect("pull");
        store.hold("sandbox-1", "alpine:3");

        // Act
        let error = store.remove("alpine:3").await.expect_err("held");

        // Assert
        assert!(matches!(error, BackendError::InvalidArgument(_)), "{error}");
        assert!(store.is_cached("alpine:3").await);
    }

    #[tokio::test]
    async fn given_an_uncached_reference_when_removed_then_it_is_not_found() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let error = store.remove("ghost").await.expect_err("missing");

        // Assert
        assert!(matches!(error, BackendError::ImageNotFound(_)), "{error}");
    }

    // ----- no detail leaks into messages ----------------------------------

    #[test]
    fn given_a_source_error_with_a_signed_url_when_mapped_then_the_message_has_no_url() {
        // Arrange
        let leaky = OciDistributionError::ServerError {
            code: 503,
            url: "https://registry.test/v2/x/blobs/sha256:aa?X-Signature=secret".to_string(),
            message: "token=secret".to_string(),
        };

        // Act
        let message = pull_error("alpine:x", leaky).to_string();

        // Assert
        assert!(
            !message.contains("secret") && !message.contains("registry.test"),
            "{message}"
        );
    }

    #[test]
    fn given_a_broken_layer_when_unpacked_then_the_message_has_no_host_path() {
        // Arrange
        let dest = TempDir::new().expect("tempdir");

        // Act
        let error =
            unpack_layer(b"not a tarball", "application/x-tar", dest.path()).expect_err("garbage");

        // Assert
        assert!(matches!(error, BackendError::Internal(_)), "{error}");
        assert!(
            !error
                .to_string()
                .contains(dest.path().to_str().expect("utf8"))
        );
    }

    #[rstest::rstest]
    #[case::too_many_requests(server_error(429))]
    #[case::server_error(server_error(500))]
    fn given_a_throttled_or_failing_registry_when_mapped_then_it_is_unavailable(
        #[case] error: OciDistributionError,
    ) {
        // Act
        let mapped = pull_error("alpine:x", error);

        // Assert
        assert!(matches!(mapped, BackendError::Unavailable(_)), "{mapped}");
    }

    #[test]
    fn given_a_client_error_when_checked_then_it_is_not_retried() {
        assert!(!is_unreachable(&server_error(400)));
        assert!(!is_unreachable(&io_error(FileTooLarge)));
        assert!(is_unreachable(&server_error(503)));
    }

    #[test]
    fn given_an_absolute_whiteout_path_when_unpacked_then_it_applies() {
        // Arrange
        let root = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(root.path().join("etc")).expect("etc");
        std::fs::write(root.path().join("etc/foo"), b"x").expect("foo");
        let layer = raw_layer_with_name("/etc/.wh.foo");

        // Act
        unpack_layer(&layer, GZIP_MEDIA, root.path()).expect("unpack");

        // Assert
        assert!(!root.path().join("etc/foo").exists());
    }

    /// A gzipped layer whose one entry has `name` written verbatim, so an
    /// absolute path survives the tar builder.
    fn raw_layer_with_name(name: &str) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        let mut tar_bytes = header.as_bytes().to_vec();
        tar_bytes.extend(vec![0u8; 1024]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_bytes).expect("gzip");
        encoder.finish().expect("finish gzip")
    }

    // ----- LRU eviction --------------------------------------------------

    #[tokio::test]
    async fn given_max_two_when_third_image_pulled_then_lru_evicted() {
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 2, Arc::new(FakePuller));

        // Pull two images, then wait a moment to ensure distinct timestamps.
        store.ensure("image-a").await.expect("a");
        // Touch "image-a" so it is more recently used than "image-b".
        store.ensure("image-b").await.expect("b");
        store.ensure("image-a").await.expect("a again");

        // Now pull a third image. LRU eviction should drop "image-b" (oldest).
        store.ensure("image-c").await.expect("c");

        // Cache count must be <= 2.
        let remaining: Vec<_> = store
            .list()
            .await
            .into_iter()
            .map(|e| e.reference)
            .collect();
        assert!(
            remaining.len() <= 2,
            "expected <= 2 cached images after eviction, got: {remaining:?}"
        );
        assert!(
            !remaining.contains(&"image-b".to_string()),
            "image-b should have been evicted (LRU), remaining: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn given_eviction_removal_fails_when_lru_runs_then_failure_is_counted_and_eviction_stays_best_effort()
     {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tempdir");
        // Generous bound: pulling two images must not trigger the
        // automatic eviction that `ensure()` runs after every pull, or the
        // manual eviction below would find nothing left to remove.
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, Arc::new(FakePuller));
        store.ensure("image-a").await.expect("a");
        store.ensure("image-b").await.expect("b");

        // Removing a directory entry needs write permission on its parent.
        // Stripping write from cache_dir makes the final rmdir of the
        // evicted entry fail deterministically, without touching the entry
        // itself (its contents are still removed before that last step).
        let original_perms = std::fs::metadata(tmp.path())
            .expect("stat cache_dir")
            .permissions();
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555))
            .expect("make cache_dir read-only");

        // A separate handle over the same cache_dir with a bound of 1, used
        // only to drive eviction directly so it has exactly one candidate.
        let evictor = ImageStore::with_puller(tmp.path().to_path_buf(), 1, Arc::new(FakePuller));
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let result = metrics::with_local_recorder(&recorder, || evictor.run_lru_eviction(""));

        // Restore permissions before any assertion can panic, so the
        // tempdir can still clean itself up on drop.
        std::fs::set_permissions(tmp.path(), original_perms).expect("restore cache_dir perms");

        result.expect("eviction failure must stay best-effort and not propagate");

        let counted = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .any(|(key, _, _, value)| {
                key.key().name() == "wardd_image_cache_eviction_failure_total"
                    && matches!(value, metrics_util::debugging::DebugValue::Counter(n) if n >= 1)
            });
        assert!(
            counted,
            "expected eviction failure counter to be incremented"
        );

        // remove_dir_all clears an entry's contents before the final rmdir
        // of the entry itself, so a failed rmdir still leaves an (empty)
        // directory behind on disk. Counting directories directly (rather
        // than through `list()`, which depends on a meta.json that's
        // already gone) proves that final removal step really failed.
        let remaining_dirs = std::fs::read_dir(tmp.path())
            .expect("read cache_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(
            remaining_dirs, 2,
            "the entry directory must still exist on disk when rmdir fails"
        );
    }

    #[tokio::test]
    async fn given_evicted_cache_entry_when_sandbox_rootfs_still_has_its_files() {
        let tmp = TempDir::new().expect("tempdir");
        // max 1 so inserting a second image evicts the first.
        let store = ImageStore::with_puller(tmp.path().to_path_buf(), 1, Arc::new(FakePuller));

        let sandbox_rootfs = tmp.path().join("sandbox1").join("rootfs");
        std::fs::create_dir_all(&sandbox_rootfs).expect("mk sandbox rootfs");

        // Pull image-a and materialize it into sandbox1.
        store
            .prepare_sandbox_rootfs("sb", "image-a", &sandbox_rootfs)
            .await
            .expect("prepare sandbox rootfs");

        // The sandbox is gone from the host's view: nothing holds the image.
        store.release("sb");

        // Verify sandbox got the materialized content.
        assert!(
            sandbox_rootfs.join("bin").exists(),
            "sandbox rootfs must have bin/ after materialization"
        );

        // Pull image-b: causes image-a to be evicted from cache.
        store.ensure("image-b").await.expect("image-b");
        assert!(
            !store.is_cached("image-a").await,
            "image-a should be evicted from cache"
        );

        // Sandbox rootfs must still have its files (the files are copies).
        assert!(
            sandbox_rootfs.join("bin").exists(),
            "sandbox rootfs must survive cache eviction"
        );
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

    // ----- materialization -----------------------------------------------

    #[tokio::test]
    async fn given_ensure_when_prepare_sandbox_rootfs_then_files_materialized() {
        let (store, tmp) = store_in_tempdir();
        let sandbox_dest = tmp.path().join("sandbox-xyz").join("rootfs");
        store
            .prepare_sandbox_rootfs("sb", "alpine:latest", &sandbox_dest)
            .await
            .expect("prepare_sandbox_rootfs");

        // FakePuller creates bin/ in the rootfs, so it must appear in dest.
        assert!(
            sandbox_dest.join("bin").exists(),
            "sandbox rootfs should contain bin/"
        );
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
        // Digest format is "sha256:<hex>" - exact value depends on FakePuller hash.
        assert!(
            entry.digest.starts_with("sha256:"),
            "expected sha256: prefix, got: {}",
            entry.digest
        );
    }

    // ----- resolve: pull policy -------------------------------------------

    const PINNED_REF: &str = "alpine@sha256:0123456789abcdef";

    /// Reports a new digest on every pull, like a tag that keeps moving.
    #[derive(Debug, Default)]
    struct MovingPuller(std::sync::atomic::AtomicU32);

    #[async_trait::async_trait]
    impl ImagePuller for MovingPuller {
        async fn pull(&self, _reference: &str, dest: &Path) -> Result<PulledImage> {
            std::fs::create_dir_all(dest.join("bin")).map_err(BackendError::Io)?;
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PulledImage::without_config(format!("sha256:{n:064x}")))
        }
    }

    #[tokio::test]
    async fn given_uncached_image_when_resolved_never_then_it_fails_like_docker() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let err = store
            .resolve("alpine", PullPolicy::Never)
            .await
            .unwrap_err();

        // Assert
        assert!(
            err.to_string()
                .contains("Unable to find image 'alpine' locally"),
            "got: {err}"
        );
        assert!(!store.is_cached("alpine").await);
    }

    #[tokio::test]
    async fn given_cached_image_when_resolved_never_then_it_returns_the_digest_without_pulling() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();
        let cached = store.pull("alpine", None).await.expect("pull");

        // Act
        let digest = store.resolve("alpine", PullPolicy::Never).await.unwrap();

        // Assert
        assert_eq!(digest, cached.digest);
    }

    #[tokio::test]
    async fn given_tag_without_digest_when_resolved_pinned_then_it_is_refused_and_not_pulled() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let err = store
            .resolve("alpine:latest", PullPolicy::Pinned)
            .await
            .unwrap_err();

        // Assert
        assert!(
            matches!(err, BackendError::InvalidArgument(_)),
            "got: {err}"
        );
        assert!(err.to_string().contains("name@sha256:"), "got: {err}");
        assert!(!store.is_cached("alpine:latest").await);
    }

    #[tokio::test]
    async fn given_digest_reference_when_resolved_pinned_then_it_is_pulled() {
        // Arrange
        let (store, _tmp) = store_in_tempdir();

        // Act
        let digest = store.resolve(PINNED_REF, PullPolicy::Pinned).await.unwrap();

        // Assert
        assert!(digest.starts_with(DIGEST_ALGORITHM_PREFIX), "got: {digest}");
        assert!(store.is_cached(PINNED_REF).await);
    }

    #[tokio::test]
    async fn given_cached_image_when_resolved_missing_then_it_is_not_pulled_again() {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(
            tmp.path().to_path_buf(),
            64,
            Arc::new(MovingPuller::default()),
        );
        let first = store.resolve("alpine", PullPolicy::Missing).await.unwrap();

        // Act
        let second = store.resolve("alpine", PullPolicy::Missing).await.unwrap();

        // Assert
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn given_cached_image_when_resolved_always_then_the_tag_moves_and_the_old_image_stays_by_digest()
     {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let store = ImageStore::with_puller(
            tmp.path().to_path_buf(),
            64,
            Arc::new(MovingPuller::default()),
        );
        let old = store.resolve("alpine", PullPolicy::Always).await.unwrap();

        // Act
        let new = store.resolve("alpine", PullPolicy::Always).await.unwrap();

        // Assert
        assert_ne!(new, old);
        let (_, tagged) = store.find_cache_entry_by_ref("alpine").unwrap().unwrap();
        assert_eq!(tagged.digest, new);
        let (_, kept) = store.find_cache_entry_by_ref(&old).unwrap().unwrap();
        assert_eq!(kept.digest, old);
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

    #[test]
    fn given_symlink_to_outside_when_whiteout_through_it_then_outside_is_untouched() {
        // Arrange
        let rootfs = TempDir::new().expect("rootfs");
        let outside = TempDir::new().expect("outside");
        std::fs::write(outside.path().join("victim"), b"host").expect("write");
        std::os::unix::fs::symlink(outside.path(), rootfs.path().join("x")).expect("symlink");

        // Act
        unpack_layer(&gzip_marker("x/.wh.victim"), GZIP_MEDIA, rootfs.path()).expect("whiteout");
        unpack_layer(&gzip_marker("x/.wh..wh..opq"), GZIP_MEDIA, rootfs.path()).expect("opaque");

        // Assert
        assert!(outside.path().join("victim").exists());
    }

    #[tokio::test]
    async fn given_a_blob_longer_than_declared_when_spooled_then_it_is_refused() {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let chunks = tokio_stream::iter([Ok(bytes::Bytes::from_static(b"ab"))]);

        // Act
        let error = spool(chunks, &tmp.path().join("blob"), 1)
            .await
            .unwrap_err();

        // Assert
        assert_eq!(error.kind(), FileTooLarge);
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
        // Trailing comma, double comma, etc. - common typos. None
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

    // ----- materialize_dir: independent copies ---------------------

    #[test]
    fn given_unreadable_cached_file_when_materialize_dir_then_copy_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        const NO_ACCESS_MODE: u32 = 0o000;
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).expect("mkdir");
        let secret = src.join("shadow");
        std::fs::write(&secret, b"root:*").expect("write");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(NO_ACCESS_MODE))
            .expect("chmod");

        materialize_dir(&src, &dst).expect("materialize");

        let mode_of = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode_of(&dst.join("shadow")), NO_ACCESS_MODE);
        assert_eq!(mode_of(&secret), NO_ACCESS_MODE, "cache mode restored");
        // Root ignores mode bits, so read back only after restoring access.
        std::fs::set_permissions(dst.join("shadow"), std::fs::Permissions::from_mode(0o600))
            .expect("chmod dst");
        assert_eq!(std::fs::read(dst.join("shadow")).expect("read"), b"root:*");
    }

    #[test]
    fn given_src_tree_when_materialize_dir_then_dst_mirrors_src() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        std::fs::write(src.join("a.txt"), b"aaa").expect("write a");
        std::fs::write(src.join("sub").join("b.txt"), b"bbb").expect("write b");

        materialize_dir(&src, &dst).expect("materialize_dir");

        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"bbb");
    }

    /// Runs `body` under a restrictive umask, as the daemon does.
    fn with_umask<T>(mask: rustix::fs::RawMode, body: impl FnOnce() -> T) -> T {
        let previous = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(mask));
        let out = body();
        rustix::process::umask(previous);
        out
    }

    #[test]
    fn given_restrictive_umask_when_materialize_dir_then_image_dir_modes_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        const DAEMON_UMASK: rustix::fs::RawMode = 0o077;
        const WORLD_SEARCH: u32 = 0o001;
        const STICKY_TMP: u32 = 0o1777;
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("sandbox/rootfs");
        for dir in ["bin", "etc", "usr/bin", "tmp"] {
            std::fs::create_dir_all(src.join(dir)).expect("mkdir");
        }
        std::fs::set_permissions(src.join("tmp"), std::fs::Permissions::from_mode(STICKY_TMP))
            .expect("chmod tmp");
        // The cache root is created by the daemon, so it is owner-only.
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o700)).expect("chmod src");

        // Act
        with_umask(DAEMON_UMASK, || {
            create_private_parent(&dst).expect("parent");
            materialize_dir(&src, &dst).expect("materialize");
        });

        // Assert
        let mode_of = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777;
        for dir in ["", "bin", "etc", "usr", "usr/bin"] {
            assert_eq!(
                mode_of(&dst.join(dir)) & WORLD_SEARCH,
                WORLD_SEARCH,
                "{dir}"
            );
        }
        assert_eq!(mode_of(&dst), ROOTFS_ROOT_MODE);
        assert_eq!(mode_of(&dst.join("tmp")), STICKY_TMP);
        assert_eq!(mode_of(dst.parent().unwrap()), SANDBOX_DIR_MODE);
    }

    #[test]
    fn given_materialized_file_when_src_modified_then_dst_unchanged_after_copy() {
        // Sandbox isolation: copies must not share an inode with the source.
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        std::fs::create_dir_all(&src).expect("mkdir src");
        std::fs::write(src.join("file.txt"), b"original").expect("write");
        materialize_dir(&src, &dst).expect("materialize");

        // Overwrite src in place - dst must keep its own content.
        std::fs::write(src.join("file.txt"), b"mutated").expect("overwrite");
        assert_eq!(std::fs::read(dst.join("file.txt")).unwrap(), b"original");

        // And the other way round: a guest edit must not reach the source.
        std::fs::write(dst.join("file.txt"), b"guest edit").expect("guest edit");
        assert_eq!(std::fs::read(src.join("file.txt")).unwrap(), b"mutated");
    }

    // ----- commit ---------------------------------------------------------

    const NO_SKIP: &[&str] = &[];

    fn sandbox_rootfs(files: &[(&str, &[u8])]) -> TempDir {
        let rootfs = TempDir::new().expect("rootfs");
        for (name, bytes) in files {
            std::fs::write(rootfs.path().join(name), bytes).expect("write");
        }
        rootfs
    }

    fn shared_store() -> (Arc<ImageStore>, TempDir) {
        let (store, tmp) = store_in_tempdir();
        (Arc::new(store), tmp)
    }

    fn spec(reference: &str) -> CommitSpec {
        CommitSpec {
            reference: Some(reference.to_string()),
            ..CommitSpec::default()
        }
    }

    #[tokio::test]
    async fn given_a_commit_after_writing_a_file_when_a_sandbox_is_prepared_from_the_tag_then_the_file_and_config_are_there()
     {
        // Arrange
        let (store, _tmp) = shared_store();
        store.pull("alpine:3", None).await.expect("pull");
        let rootfs = sandbox_rootfs(&[("data", b"known bytes")]);
        let commit = CommitSpec {
            author: "Ada".into(),
            message: "wip".into(),
            changes: vec!["ENV A=1".into()],
            ..spec("mine:v1")
        };

        // Act
        let image = store
            .commit(rootfs.path(), fake_config(), &commit, NO_SKIP)
            .await
            .expect("commit");
        let dest = TempDir::new().expect("dest");
        store
            .prepare_sandbox_rootfs("sb", "mine:v1", dest.path())
            .await
            .expect("prepare");
        let config = store.config("mine:v1").await.expect("config");

        // Assert
        assert_eq!(
            std::fs::read(dest.path().join("data")).unwrap(),
            b"known bytes"
        );
        assert!(image.digest.starts_with(DIGEST_ALGORITHM_PREFIX));
        assert_eq!(config.cmd, fake_config().cmd);
        assert_eq!(config.entrypoint, fake_config().entrypoint);
        assert_eq!(config.env, ["PATH=/bin", "A=1"]);
        let listed: Vec<_> = store
            .list()
            .await
            .into_iter()
            .map(|i| i.reference)
            .collect();
        assert!(listed.contains(&"mine:v1".to_string()), "{listed:?}");
        let (_, meta) = store.find_cache_entry_by_ref("mine:v1").unwrap().unwrap();
        assert_eq!(meta.author.as_deref(), Some("Ada"));
        assert_eq!(meta.comment.as_deref(), Some("wip"));
    }

    #[tokio::test]
    async fn given_a_pulled_tag_when_committed_to_it_then_the_tag_moves_and_the_old_image_stays_by_digest()
     {
        // Arrange
        let (store, _tmp) = shared_store();
        let old = store.pull("alpine", None).await.expect("pull");
        let rootfs = sandbox_rootfs(&[("data", b"x")]);

        // Act
        let new = store
            .commit(
                rootfs.path(),
                ImageConfig::default(),
                &spec("alpine"),
                NO_SKIP,
            )
            .await
            .expect("commit");

        // Assert
        let (_, tagged) = store.find_cache_entry_by_ref("alpine").unwrap().unwrap();
        assert_eq!(tagged.digest, new.digest);
        let (_, kept) = store.find_cache_entry_by_ref(&old.digest).unwrap().unwrap();
        assert_eq!(kept.digest, old.digest);
        assert!(!kept.committed);
    }

    #[tokio::test]
    async fn given_a_committed_image_when_more_images_are_pulled_than_the_cache_holds_then_it_survives()
     {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        let store = Arc::new(ImageStore::with_puller(
            tmp.path().to_path_buf(),
            2,
            Arc::new(FakePuller),
        ));
        let rootfs = sandbox_rootfs(&[("data", b"x")]);
        store
            .commit(
                rootfs.path(),
                ImageConfig::default(),
                &spec("mine:v1"),
                NO_SKIP,
            )
            .await
            .expect("commit");

        // Act
        for image in ["a:1", "b:1", "c:1"] {
            store.pull(image, None).await.expect("pull");
        }

        // Assert
        assert!(store.is_cached("mine:v1").await);
        assert!(!store.is_cached("a:1").await);
    }

    #[test]
    fn given_leftover_staging_dirs_when_the_store_starts_then_they_are_removed() {
        // Arrange
        let tmp = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".tmp-stale/rootfs")).expect("stale");

        // Act
        let _store = ImageStore::new(tmp.path().to_path_buf(), 64);

        // Assert
        assert!(cache_dir_names(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn given_a_bad_tag_when_committed_then_it_is_rejected_before_any_copy() {
        for tag in ["../../x", "/abs/path", "a/../b", "Not A Ref"] {
            // Arrange
            let (store, tmp) = shared_store();
            let missing = tmp.path().join("no-such-rootfs");

            // Act
            let error = store
                .commit(&missing, ImageConfig::default(), &spec(tag), NO_SKIP)
                .await
                .expect_err("bad tag");

            // Assert
            assert!(
                matches!(error, BackendError::InvalidArgument(_)),
                "{tag}: {error}"
            );
            assert!(cache_dir_names(tmp.path()).is_empty(), "{tag}");
        }
    }

    #[tokio::test]
    async fn given_a_rootfs_that_would_pass_the_cache_cap_when_committed_then_it_is_full_and_unchanged()
     {
        // Arrange
        let (store, tmp) = {
            let tmp = TempDir::new().expect("tempdir");
            let store = ImageStore::with_puller(tmp.path().to_path_buf(), 64, Arc::new(FakePuller))
                .with_limits(u64::MAX, SMALL_CAP);
            (Arc::new(store), tmp)
        };
        let rootfs = sandbox_rootfs(&[("big", &[0u8; SMALL_CAP as usize + 1])]);

        // Act
        let error = store
            .commit(
                rootfs.path(),
                ImageConfig::default(),
                &spec("big:v1"),
                NO_SKIP,
            )
            .await
            .expect_err("cache full");

        // Assert
        assert!(
            matches!(error, BackendError::ResourceExhausted(_)),
            "{error}"
        );
        assert!(error.to_string().contains("cache is full"), "{error}");
        assert!(cache_dir_names(tmp.path()).is_empty());
    }

    #[tokio::test]
    async fn given_an_unsupported_change_when_committed_then_it_is_rejected_and_nothing_is_cached()
    {
        // Arrange
        let (store, tmp) = shared_store();
        let rootfs = sandbox_rootfs(&[]);
        let commit = CommitSpec {
            changes: vec!["EXPOSE 80".into()],
            ..spec("x:v1")
        };

        // Act
        let error = store
            .commit(rootfs.path(), ImageConfig::default(), &commit, NO_SKIP)
            .await
            .expect_err("unsupported");

        // Assert
        assert!(error.to_string().contains("unsupported change"), "{error}");
        assert!(cache_dir_names(tmp.path()).is_empty());
    }
}
