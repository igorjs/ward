// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! What the real `OciPuller` sends to a registry and its token service on
//! loopback, with a stored login. The registry is reached over plain HTTP, so
//! these tests run with the operator-configured plain-HTTP exception
//! (`OciPuller::with_http_registry`): an http realm is acceptable only on
//! that registry's own host and port. Every other http realm is a realm a
//! real https registry could name, and the login must not reach it.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use ward_core::backend::image::{ImagePuller, OciPuller, PullSession};
use ward_core::registry_auth::{self, Credentials};

const USERNAME: &str = "ada";
const PASSWORD: &str = "s3cret-pw";
const OTHER_PASSWORD: &str = "other-pw";
const REPOSITORY: &str = "team/app";
const HEADER_END: &[u8] = b"\r\n\r\n";
const LONG_LIVED_TOKEN_SECS: u64 = 300;
/// Below the floor Ward raises a token lifetime to.
const TINY_TOKEN_SECS: u64 = 1;
const FIFO_HOLD: Duration = Duration::from_millis(400);
const TICK: Duration = Duration::from_millis(50);
const OCI_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Tests share the process environment, so they run one after another.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug)]
struct Recorded {
    target: String,
    authorization: Option<String>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

type Handler = Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>;

struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    async fn start(make: impl FnOnce(SocketAddr) -> Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handler = make(addr);
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let log = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, handler.clone(), log.clone()));
            }
        });
        Self { addr, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("log").clone()
    }

    fn count(&self, prefix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.target.starts_with(prefix))
            .count()
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    handler: Handler,
    log: Arc<Mutex<Vec<Recorded>>>,
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
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let authorization = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.trim().to_string());
    let recorded = Recorded {
        target: target.to_string(),
        authorization,
    };
    log.lock().expect("log").push(recorded.clone());
    let reply = handler(&recorded);
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    if method != "HEAD" {
        let _ = stream.write_all(&reply.body).await;
    }
    let _ = stream.shutdown().await;
}

fn basic(password: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{password}"));
    format!("Basic {encoded}")
}

fn sha256_digest(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// A manifest with its config and layer blobs, as the registry serves them.
struct Published {
    body: String,
    digest: String,
    blobs: HashMap<String, Vec<u8>>,
    first_layer_path: Option<String>,
}

fn publish(layers: &[Vec<u8>]) -> Published {
    let config = br#"{"config":{"Cmd":["/bin/sh"]}}"#.to_vec();
    let mut blobs = HashMap::new();
    let mut descriptor = |media_type: &str, bytes: &[u8]| {
        let digest = sha256_digest(bytes);
        blobs.insert(format!("/v2/{REPOSITORY}/blobs/{digest}"), bytes.to_vec());
        serde_json::json!({"mediaType": media_type, "digest": digest, "size": bytes.len()})
    };
    let config_descriptor = descriptor("application/vnd.oci.image.config.v1+json", &config);
    let layer_descriptors: Vec<_> = layers
        .iter()
        .map(|layer| descriptor("application/vnd.oci.image.layer.v1.tar+gzip", layer))
        .collect();
    let first_layer_path = layer_descriptors.first().map(|d| {
        format!(
            "/v2/{REPOSITORY}/blobs/{}",
            d["digest"].as_str().unwrap_or_default()
        )
    });
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST_TYPE,
        "config": config_descriptor,
        "layers": layer_descriptors,
    })
    .to_string();
    let digest = sha256_digest(body.as_bytes());
    Published {
        body,
        digest,
        blobs,
        first_layer_path,
    }
}

fn manifest() -> (String, String) {
    let published = publish(&[]);
    (published.body, published.digest)
}

/// A gzip tar layer holding one file.
fn layer(path: &str, contents: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, path, contents.as_bytes())
        .expect("file");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&builder.into_inner().expect("tar"))
        .expect("gzip");
    encoder.finish().expect("gzip")
}

/// What a test varies in the registry. Its token service lives on the same
/// server at `/token` and issues `tok-N` for the stored login, `anon-N` for
/// no login and nothing for a wrong one.
#[derive(Clone)]
struct RegistryPlan {
    /// The `WWW-Authenticate` value for `/v2/`, given the registry's address.
    challenge: Arc<dyn Fn(SocketAddr) -> Option<String> + Send + Sync>,
    /// What `/v2/` answers with instead of a challenge.
    v2_redirect: Option<String>,
    /// `expires_in` of every token issued.
    expires_in: u64,
    /// Tokens the manifest endpoint refuses, as a revoked token would be.
    revoked: Arc<Mutex<Vec<String>>>,
    /// Where `/token` redirects to, instead of answering.
    token_redirect: Option<String>,
    /// Answer `/token` with this status instead of a token.
    token_status: Option<u16>,
    /// Answer `/v2/` with 404 and no challenge while set.
    v2_broken: Arc<AtomicBool>,
    minted: Arc<AtomicUsize>,
    password: Arc<Mutex<String>>,
    /// Layers of the image, bottom first.
    layers: Vec<Vec<u8>>,
    /// Refuse the token that fetched the first layer from then on.
    revoke_after_first_layer: bool,
}

impl RegistryPlan {
    fn new(challenge: impl Fn(SocketAddr) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            challenge: Arc::new(challenge),
            v2_redirect: None,
            expires_in: LONG_LIVED_TOKEN_SECS,
            revoked: Arc::default(),
            token_redirect: None,
            token_status: None,
            v2_broken: Arc::default(),
            minted: Arc::default(),
            password: Arc::new(Mutex::new(PASSWORD.to_string())),
            layers: Vec::new(),
            revoke_after_first_layer: false,
        }
    }

    /// A Bearer challenge whose realm is the registry's own `/token`.
    fn own_realm() -> Self {
        Self::new(|addr| {
            Some(format!(
                r#"Bearer realm="http://{addr}/token",service="registry""#
            ))
        })
    }

    fn realm_text(realm: impl Fn(SocketAddr) -> String + Send + Sync + 'static) -> Self {
        Self::new(move |addr| {
            Some(format!(
                r#"Bearer realm="{}",service="registry""#,
                realm(addr)
            ))
        })
    }

    async fn start(&self) -> Server {
        let plan = self.clone();
        Server::start(move |addr| {
            let published = publish(&plan.layers);
            Arc::new(move |req: &Recorded| plan.answer(addr, req, &published))
        })
        .await
    }

    fn answer(&self, addr: SocketAddr, req: &Recorded, published: &Published) -> Reply {
        let challenge = (self.challenge)(addr);
        let unauthorized = || {
            // One header per line of the challenge text.
            challenge
                .iter()
                .flat_map(|value| value.lines())
                .fold(Reply::new(401), |reply, line| {
                    reply.header("WWW-Authenticate", line)
                })
        };
        if req.target == "/v2/" {
            if self.v2_broken.load(Ordering::SeqCst) {
                return Reply::new(404);
            }
            if let Some(location) = &self.v2_redirect {
                return Reply::new(307).header("Location", location.clone());
            }
            return if challenge.is_some() {
                unauthorized()
            } else {
                Reply::new(200).body("{}")
            };
        }
        if req.target == "/elsewhere" {
            return unauthorized();
        }
        if req.target.starts_with("/token") {
            if let Some(location) = &self.token_redirect {
                return Reply::new(307).header("Location", location.clone());
            }
            if let Some(status) = self.token_status {
                return Reply::new(status);
            }
            let current = basic(&self.password.lock().expect("password"));
            let n = self.minted.fetch_add(1, Ordering::SeqCst) + 1;
            let token = match req.authorization.as_deref() {
                Some(value) if value == current => format!("tok-{n}"),
                None => format!("anon-{n}"),
                Some(_) => return Reply::new(401),
            };
            return Reply::new(200).body(
                serde_json::json!({"token": token, "expires_in": self.expires_in}).to_string(),
            );
        }
        let bearer = req
            .authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "));
        let Some(token) =
            bearer.filter(|t| !self.revoked.lock().expect("revoked").iter().any(|r| r == t))
        else {
            return unauthorized();
        };
        if let Some(blob) = published.blobs.get(&req.target) {
            if self.revoke_after_first_layer
                && published.first_layer_path.as_deref() == Some(req.target.as_str())
            {
                self.revoked
                    .lock()
                    .expect("revoked")
                    .push(token.to_string());
            }
            return Reply::new(200)
                .header("Content-Type", "application/octet-stream")
                .header("Docker-Content-Digest", sha256_digest(blob))
                .body(blob.clone());
        }
        Reply::new(200)
            .header("Content-Type", OCI_MANIFEST_TYPE)
            .header("Docker-Content-Digest", published.digest.clone())
            .body(published.body.clone())
    }
}

/// A token service on its own port, which answers every request with a token.
async fn other_realm() -> Server {
    Server::start(|_| {
        Arc::new(|_: &Recorded| Reply::new(200).body(r#"{"token":"from-other","expires_in":300}"#))
    })
    .await
}

struct Env {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        let guard = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: ENV_LOCK serialises every test that touches the variable.
        unsafe { std::env::set_var("DOCKER_CONFIG", dir.path()) };
        Self {
            _guard: guard,
            _dir: dir,
        }
    }

    fn dir(&self) -> &std::path::Path {
        self._dir.path()
    }

    fn log_in(&self, registry: SocketAddr, password: &str) {
        registry_auth::store(
            &registry.to_string(),
            &Credentials {
                username: USERNAME.to_string(),
                password: password.to_string(),
            },
        )
        .expect("store login");
    }

    fn log_out(&self, registry: SocketAddr) {
        registry_auth::erase(&registry.to_string()).expect("erase login");
    }
}

fn reference(registry: SocketAddr) -> String {
    format!("{registry}/{REPOSITORY}:latest")
}

/// Every request, on any server, that carried a Basic header with `password`.
fn requests_with_password(servers: &[&Server], password: &str) -> Vec<Recorded> {
    let wanted = basic(password);
    servers
        .iter()
        .flat_map(|s| s.requests())
        .filter(|r| r.authorization.as_deref() == Some(wanted.as_str()))
        .collect()
}

fn any_basic(servers: &[&Server]) -> Vec<Recorded> {
    servers
        .iter()
        .flat_map(|s| s.requests())
        .filter(|r| {
            r.authorization
                .as_deref()
                .is_some_and(|v| v.starts_with("Basic "))
        })
        .collect()
}

#[tokio::test]
async fn given_a_same_host_http_realm_on_a_configured_http_registry_when_resolved_then_only_the_token_request_carries_the_login()
 {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::own_realm().start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let digest = puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("resolve");

    // Assert
    assert_eq!(digest, Some(manifest().1));
    let with_password = requests_with_password(&[&registry], PASSWORD);
    assert_eq!(with_password.len(), 1, "{with_password:?}");
    assert!(with_password[0].target.starts_with("/token"));
    assert!(
        with_password[0]
            .target
            .contains("scope=repository%3Ateam%2Fapp%3Apull")
    );
    let manifest_request = registry
        .requests()
        .into_iter()
        .find(|r| r.target.contains("/manifests/"))
        .expect("manifest request");
    assert_eq!(
        manifest_request.authorization.as_deref(),
        Some("Bearer tok-1")
    );
    let shown = format!("{puller:?}");
    assert!(
        !shown.contains(PASSWORD) && !shown.contains("tok-1"),
        "{shown}"
    );
}

#[tokio::test]
async fn given_two_calls_when_the_token_is_long_lived_then_one_client_and_one_token_serve_both() {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::own_realm().start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("first");
    puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("second");

    // Assert
    let probes = registry
        .requests()
        .iter()
        .filter(|r| r.target == "/v2/")
        .count();
    assert_eq!(probes, 1);
    assert_eq!(registry.count("/token"), 1);
    assert_eq!(registry.count("/v2/team"), 2);
}

#[tokio::test]
async fn given_a_token_with_a_tiny_lifetime_when_the_next_call_starts_then_the_floor_keeps_it_in_use()
 {
    // Arrange
    let env = Env::new().await;
    let mut plan = RegistryPlan::own_realm();
    plan.expires_in = TINY_TOKEN_SECS;
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    for _ in 0..3 {
        puller
            .resolve_digest(&reference(registry.addr))
            .await
            .expect("resolve");
    }

    // Assert
    assert_eq!(registry.count("/token"), 1);
}

#[tokio::test]
async fn given_a_token_the_registry_revoked_when_a_request_gets_401_then_a_new_token_is_used_once()
{
    // Arrange
    let env = Env::new().await;
    let plan = RegistryPlan::own_realm();
    plan.revoked.lock().expect("revoked").push("tok-1".into());
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string())
        .with_token_request_interval(Duration::ZERO);

    // Act
    let digest = puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("resolve");

    // Assert
    assert_eq!(digest, Some(manifest().1));
    assert_eq!(registry.count("/token"), 2);
    assert_eq!(requests_with_password(&[&registry], PASSWORD).len(), 2);
}

/// Resolve against a registry whose realm is `realm_for(other)`, a plain-HTTP
/// token service the configured registry does not own, and check the login
/// went nowhere.
async fn assert_login_withheld_from(
    realm_for: impl Fn(SocketAddr) -> String + Send + Sync + 'static,
) {
    // Arrange
    let env = Env::new().await;
    let other = other_realm().await;
    let other_addr = other.addr;
    let registry = RegistryPlan::realm_text(move |_| realm_for(other_addr))
        .start()
        .await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(any_basic(&[&registry, &other]).is_empty());
    assert!(
        other.requests().iter().all(|r| r.authorization.is_none()),
        "{:?}",
        other.requests()
    );
}

#[tokio::test]
async fn given_an_http_realm_on_another_host_when_resolved_then_the_login_is_not_sent_and_the_pull_is_anonymous()
 {
    assert_login_withheld_from(|other| format!("http://{other}/token")).await;
}

#[tokio::test]
async fn given_a_realm_written_with_escapes_when_resolved_then_the_login_is_not_sent() {
    assert_login_withheld_from(|other| format!(r"\h\t\t\p://{other}/token")).await;
}

#[tokio::test]
async fn given_a_realm_with_a_tab_in_its_scheme_when_resolved_then_the_login_is_not_sent() {
    assert_login_withheld_from(|other| format!("ht\ttp://{other}/token")).await;
}

/// The redirect target answers with a challenge Ward would accept (the
/// registry's own realm). Ward's probe does not follow redirects, so it sees no
/// challenge and sends no login; the explicit redirect rule in `plan_login` is
/// defence in depth, and this test does not tell the two apart. The pull itself
/// runs anonymously through oci-client, which follows the redirect.
#[tokio::test]
async fn given_a_v2_endpoint_that_redirects_to_an_acceptable_realm_when_resolved_then_no_login_is_sent()
 {
    // Arrange
    let env = Env::new().await;
    let mut plan = RegistryPlan::own_realm();
    plan.v2_redirect = Some("/elsewhere".to_string());
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(
        any_basic(&[&registry]).is_empty(),
        "{:?}",
        registry.requests()
    );
    assert!(
        registry.count("/elsewhere") >= 1,
        "oci-client followed the redirect"
    );
    let token_requests: Vec<_> = registry
        .requests()
        .into_iter()
        .filter(|r| r.target.starts_with("/token"))
        .collect();
    assert!(token_requests.iter().all(|r| r.authorization.is_none()));
}

/// A realm written with escapes or a tab that, once parsed, names the
/// registry's own host is accepted and used. A wrong parse would judge or
/// request a different URL and the login would not reach `/token`.
async fn assert_realm_judged_and_used_as_parsed(
    realm_for: impl Fn(SocketAddr) -> String + Send + Sync + 'static,
) {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::realm_text(realm_for).start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let digest = puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("resolve");

    // Assert
    assert_eq!(digest, Some(manifest().1));
    let with_password = requests_with_password(&[&registry], PASSWORD);
    assert_eq!(with_password.len(), 1, "{:?}", registry.requests());
    assert!(with_password[0].target.starts_with("/token"));
}

#[tokio::test]
async fn given_an_escaped_realm_naming_the_registrys_own_host_when_resolved_then_the_unescaped_url_is_used()
 {
    assert_realm_judged_and_used_as_parsed(|own| format!(r"\h\t\t\p://{own}/token")).await;
}

#[tokio::test]
async fn given_a_realm_with_a_tab_in_its_scheme_naming_the_registrys_own_host_when_resolved_then_the_parsed_url_is_used()
 {
    assert_realm_judged_and_used_as_parsed(|own| format!("ht\ttp://{own}/token")).await;
}

#[tokio::test]
async fn given_a_basic_only_registry_when_resolved_then_no_login_is_sent() {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::new(|_| Some(r#"Basic realm="registry""#.to_string()))
        .start()
        .await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let _ = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(
        any_basic(&[&registry]).is_empty(),
        "{:?}",
        registry.requests()
    );
}

#[tokio::test]
async fn given_a_token_service_that_redirects_when_resolved_then_the_login_goes_nowhere_else() {
    // Arrange
    let env = Env::new().await;
    let other = other_realm().await;
    let mut plan = RegistryPlan::own_realm();
    plan.token_redirect = Some(format!("http://{}/token", other.addr));
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_err());
    assert!(other.requests().is_empty(), "{:?}", other.requests());
}

#[tokio::test]
async fn given_a_login_that_changes_or_goes_when_the_next_call_starts_then_it_is_honoured() {
    // Arrange
    let env = Env::new().await;
    let plan = RegistryPlan::own_realm();
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());
    puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("first");

    // Act
    *plan.password.lock().expect("password") = OTHER_PASSWORD.to_string();
    env.log_in(registry.addr, OTHER_PASSWORD);
    puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("after login change");
    let before_logout = registry.count("/v2/team");
    env.log_out(registry.addr);
    puller
        .resolve_digest(&reference(registry.addr))
        .await
        .expect("after logout");

    // Assert
    assert_eq!(requests_with_password(&[&registry], PASSWORD).len(), 1);
    assert_eq!(
        requests_with_password(&[&registry], OTHER_PASSWORD).len(),
        1
    );
    let last_manifest = registry
        .requests()
        .into_iter()
        .rfind(|r| r.target.contains("/manifests/"))
        .expect("manifest request");
    assert_eq!(registry.count("/v2/team"), before_logout + 1);
    let bearer = last_manifest.authorization.expect("authorization");
    assert!(bearer.starts_with("Bearer anon-"), "{bearer}");
}

#[tokio::test]
async fn given_check_login_when_the_realm_is_the_registrys_own_then_a_good_login_passes_and_a_bad_one_fails()
 {
    // Arrange
    let _env = Env::new().await;
    let registry = RegistryPlan::own_realm().start().await;
    let puller = OciPuller::with_http_registry(registry.addr.to_string());
    let login = |password: &str| Credentials {
        username: USERNAME.to_string(),
        password: password.to_string(),
    };
    let server = registry.addr.to_string();

    // Act
    let good = puller.check_login(&server, &login(PASSWORD)).await;
    let bad = puller.check_login(&server, &login("wrong")).await;

    // Assert
    assert!(good.is_ok(), "{good:?}");
    let message = bad.expect_err("wrong password").to_string();
    assert!(message.contains("did not accept"), "{message}");
    assert!(!message.contains("wrong"));
}

#[tokio::test]
async fn given_check_login_when_the_realm_is_another_http_host_then_it_is_refused_and_nothing_is_sent()
 {
    // Arrange
    let _env = Env::new().await;
    let other = other_realm().await;
    let other_addr = other.addr;
    let registry = RegistryPlan::realm_text(move |_| format!("http://{other_addr}/token"))
        .start()
        .await;
    let puller = OciPuller::with_http_registry(registry.addr.to_string());
    let login = Credentials {
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
    };

    // Act
    let outcome = puller.check_login(&registry.addr.to_string(), &login).await;

    // Assert
    let message = outcome.expect_err("refused").to_string();
    assert!(message.contains("refused"), "{message}");
    assert!(!message.contains(PASSWORD));
    assert!(other.requests().is_empty());
}

#[tokio::test]
async fn given_a_token_that_ends_between_layers_when_pulled_then_one_new_token_completes_the_pull()
{
    // Arrange
    let env = Env::new().await;
    let mut plan = RegistryPlan::own_realm();
    plan.layers = vec![layer("one.txt", "1"), layer("two.txt", "2")];
    plan.revoke_after_first_layer = true;
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string())
        .with_token_request_interval(Duration::ZERO);
    let staging = tempfile::tempdir().expect("tempdir");
    let dest = staging.path().join("rootfs");
    std::fs::create_dir_all(&dest).expect("rootfs");
    let mut session = PullSession::new(u64::MAX, u64::MAX, None);

    // Act
    let pulled = puller
        .pull_streaming(&reference(registry.addr), &dest, &mut session)
        .await
        .expect("the pull completes");

    // Assert
    assert_eq!(pulled.digest, publish(&plan.layers).digest);
    assert_eq!(
        std::fs::read_to_string(dest.join("one.txt")).expect("layer one"),
        "1"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("two.txt")).expect("layer two"),
        "2"
    );
    assert_eq!(registry.count("/token"), 2);
    let blob_tokens: Vec<_> = registry
        .requests()
        .into_iter()
        .filter(|r| r.target.contains("/blobs/"))
        .filter_map(|r| r.authorization)
        .collect();
    assert_eq!(blob_tokens.last().map(String::as_str), Some("Bearer tok-2"));
    let with_password = requests_with_password(&[&registry], PASSWORD);
    assert!(with_password.iter().all(|r| r.target.starts_with("/token")));
}

#[tokio::test]
async fn given_a_401_right_after_a_fresh_token_when_resolved_then_the_login_is_not_sent_again() {
    // Arrange
    let env = Env::new().await;
    let plan = RegistryPlan::own_realm();
    plan.revoked.lock().expect("revoked").push("tok-1".into());
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_err());
    assert_eq!(registry.count("/token"), 1);
}

#[tokio::test]
async fn given_a_v2_endpoint_that_fails_without_a_challenge_when_resolved_then_nothing_is_cached() {
    // Arrange
    let env = Env::new().await;
    let plan = RegistryPlan::own_realm();
    plan.v2_broken.store(true, Ordering::SeqCst);
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let failed = puller.resolve_digest(&reference(registry.addr)).await;
    plan.v2_broken.store(false, Ordering::SeqCst);
    let recovered = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(failed.is_err());
    assert!(recovered.is_ok(), "{recovered:?}");
    assert_eq!(requests_with_password(&[&registry], PASSWORD).len(), 1);
    let manifest_request = registry
        .requests()
        .into_iter()
        .find(|r| r.target.contains("/manifests/"))
        .expect("manifest request");
    assert_eq!(
        manifest_request.authorization.as_deref(),
        Some("Bearer tok-1")
    );
}

#[tokio::test]
async fn given_check_login_against_a_basic_challenge_when_run_then_it_is_refused_and_says_why() {
    // Arrange
    let _env = Env::new().await;
    let registry = RegistryPlan::new(|_| Some(r#"Basic realm="registry""#.to_string()))
        .start()
        .await;
    let puller = OciPuller::with_http_registry(registry.addr.to_string());
    let login = Credentials {
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
    };

    // Act
    let outcome = puller.check_login(&registry.addr.to_string(), &login).await;

    // Assert
    let message = outcome.expect_err("refused").to_string();
    assert!(
        message.contains("refused") && message.contains("Basic"),
        "{message}"
    );
    assert!(any_basic(&[&registry]).is_empty());
}

#[tokio::test]
async fn given_a_token_service_that_answers_403_when_checking_a_login_then_the_credentials_are_refused()
 {
    // Arrange
    let _env = Env::new().await;
    let mut plan = RegistryPlan::own_realm();
    plan.token_status = Some(403);
    let registry = plan.start().await;
    let puller = OciPuller::with_http_registry(registry.addr.to_string());
    let login = Credentials {
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
    };

    // Act
    let outcome = puller.check_login(&registry.addr.to_string(), &login).await;

    // Assert
    let message = outcome.expect_err("refused").to_string();
    assert!(message.contains("did not accept"), "{message}");
}

#[tokio::test]
async fn given_a_basic_header_before_a_bearer_one_when_resolved_then_the_bearer_realm_is_used() {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::new(|addr| {
        Some(format!(
            "Basic realm=\"registry\"\nBearer realm=\"http://{addr}/token\",service=\"registry\""
        ))
    })
    .start()
    .await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(requests_with_password(&[&registry], PASSWORD).len(), 1);
}

#[tokio::test]
async fn given_a_bad_bearer_header_before_a_good_one_when_resolved_then_the_login_is_withheld() {
    // Arrange
    let env = Env::new().await;
    let other = other_realm().await;
    let other_addr = other.addr;
    let registry = RegistryPlan::new(move |addr| {
        Some(format!(
            "Bearer realm=\"http://{other_addr}/token\"\nBearer realm=\"http://{addr}/token\""
        ))
    })
    .start()
    .await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let _ = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(any_basic(&[&registry, &other]).is_empty());
    assert!(other.requests().iter().all(|r| r.authorization.is_none()));
}

/// The login file is a FIFO nobody has opened, so reading it blocks. On a
/// single-thread runtime a timer still has to fire while it does.
#[tokio::test(flavor = "current_thread")]
async fn given_a_login_file_that_blocks_when_resolved_then_the_runtime_keeps_running() {
    // Arrange
    let env = Env::new().await;
    let registry = RegistryPlan::new(|_| None).start().await;
    let fifo = env.dir().join("config.json");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let released = Arc::new(AtomicBool::new(false));
    let writer = {
        let released = released.clone();
        std::thread::spawn(move || {
            std::thread::sleep(FIFO_HOLD);
            released.store(true, Ordering::SeqCst);
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        })
    };
    let puller = Arc::new(OciPuller::with_http_registry(registry.addr.to_string()));
    let resolving = tokio::spawn({
        let (puller, reference) = (puller.clone(), reference(registry.addr));
        async move {
            let _ = puller.resolve_digest(&reference).await;
        }
    });

    // Act
    tokio::time::sleep(TICK).await;
    let released_when_the_timer_fired = released.load(Ordering::SeqCst);
    resolving.await.expect("resolve task");
    writer.join().expect("writer");

    // Assert
    assert!(
        !released_when_the_timer_fired,
        "the login lookup blocked the runtime"
    );
}
