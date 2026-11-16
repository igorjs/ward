// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! What the real `OciPuller` sends to a registry and its token service on
//! loopback, with a stored login. The registry is reached over plain HTTP, so
//! these tests run with the operator-configured plain-HTTP exception
//! (`OciPuller::with_http_registry`): an http realm is acceptable only on
//! that registry's own host and port. Every other http realm is a realm a
//! real https registry could name, and the login must not reach it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use ward_core::backend::image::{ImagePuller, OciPuller};
use ward_core::registry_auth::{self, Credentials};

const USERNAME: &str = "ada";
const PASSWORD: &str = "s3cret-pw";
const OTHER_PASSWORD: &str = "other-pw";
const REPOSITORY: &str = "team/app";
const HEADER_END: &[u8] = b"\r\n\r\n";
const LONG_LIVED_TOKEN_SECS: u64 = 300;
/// Shorter than the margin before which Ward replaces a token.
const SHORT_LIVED_TOKEN_SECS: u64 = 5;
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
    body: String,
}

impl Reply {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    fn body(mut self, body: impl Into<String>) -> Self {
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
        let _ = stream.write_all(reply.body.as_bytes()).await;
    }
    let _ = stream.shutdown().await;
}

fn basic(password: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{USERNAME}:{password}"));
    format!("Basic {encoded}")
}

fn manifest() -> (String, String) {
    let body = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST_TYPE,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{}", "0".repeat(64)),
            "size": 2,
        },
        "layers": [],
    })
    .to_string();
    let hex: String = Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (body, format!("sha256:{hex}"))
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
    minted: Arc<AtomicUsize>,
    password: Arc<Mutex<String>>,
}

impl RegistryPlan {
    fn new(challenge: impl Fn(SocketAddr) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            challenge: Arc::new(challenge),
            v2_redirect: None,
            expires_in: LONG_LIVED_TOKEN_SECS,
            revoked: Arc::default(),
            token_redirect: None,
            minted: Arc::default(),
            password: Arc::new(Mutex::new(PASSWORD.to_string())),
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
            let (body, digest) = manifest();
            Arc::new(move |req: &Recorded| plan.answer(addr, req, &body, &digest))
        })
        .await
    }

    fn answer(&self, addr: SocketAddr, req: &Recorded, body: &str, digest: &str) -> Reply {
        let challenge = (self.challenge)(addr);
        let unauthorized = || {
            let reply = Reply::new(401);
            match &challenge {
                Some(value) => reply.header("WWW-Authenticate", value.clone()),
                None => reply,
            }
        };
        if req.target == "/v2/" {
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
        match bearer {
            Some(token)
                if !self
                    .revoked
                    .lock()
                    .expect("revoked")
                    .iter()
                    .any(|r| r == token) =>
            {
                Reply::new(200)
                    .header("Content-Type", OCI_MANIFEST_TYPE)
                    .header("Docker-Content-Digest", digest)
                    .body(body)
            }
            _ => unauthorized(),
        }
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
async fn given_a_token_near_its_end_when_the_next_call_starts_then_a_new_token_is_fetched() {
    // Arrange
    let env = Env::new().await;
    let mut plan = RegistryPlan::own_realm();
    plan.expires_in = SHORT_LIVED_TOKEN_SECS;
    let registry = plan.start().await;
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
    assert_eq!(registry.count("/token"), 2);
    let bearers: Vec<_> = registry
        .requests()
        .into_iter()
        .filter(|r| r.target.contains("/manifests/"))
        .filter_map(|r| r.authorization)
        .collect();
    assert_eq!(bearers, ["Bearer tok-1", "Bearer tok-2"]);
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
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

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

#[tokio::test]
async fn given_a_v2_endpoint_that_redirects_when_resolved_then_the_login_is_not_sent_anywhere() {
    // Arrange
    let env = Env::new().await;
    let other = other_realm().await;
    let other_addr = other.addr;
    let mut plan = RegistryPlan::realm_text(move |_| format!("http://{other_addr}/token"));
    plan.v2_redirect = Some("/elsewhere".to_string());
    let registry = plan.start().await;
    env.log_in(registry.addr, PASSWORD);
    let puller = OciPuller::with_http_registry(registry.addr.to_string());

    // Act
    let outcome = puller.resolve_digest(&reference(registry.addr)).await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(any_basic(&[&registry, &other]).is_empty());
    assert!(
        registry.count("/elsewhere") >= 1,
        "oci-client followed the redirect"
    );
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
