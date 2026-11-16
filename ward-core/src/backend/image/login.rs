// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Registry login. Ward asks the registry's token service for a token itself
//! and hands oci-client only that token. oci-client would otherwise send the
//! password to whatever realm a `WWW-Authenticate` header names, every time
//! it needs a token, and without checking the realm's scheme.

use std::time::Duration;

use http_auth::ChallengeParser;
use oci_client::errors::OciDistributionError;
use reqwest::Url;

use super::{REGISTRY_CONNECT_TIMEOUT, REGISTRY_READ_TIMEOUT};
use crate::registry_auth::Credentials;

/// A token service answer is a small JSON object.
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;
/// The lifetime Docker's token spec assumes when `expires_in` is absent.
const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(60);
const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);
/// A shorter `expires_in` is raised to this, so a token service cannot make
/// Ward send the password again for every request.
const MIN_TOKEN_LIFETIME: Duration = Duration::from_secs(60);
/// The longest one login request may take in all, however slowly the body
/// arrives. The read timeout alone restarts with every byte.
pub(super) const LOGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const BASIC_SCHEME: &str = "Basic";
const BEARER_SCHEME: &str = "Bearer";
const HTTPS_SCHEME: &str = "https";
const HTTP_SCHEME: &str = "http";

/// What to do about a registry's login.
pub(super) enum LoginPlan {
    /// The registry asked for no authentication, so no login is sent.
    NoLoginNeeded,
    /// A token the registry's token service issued for the stored login.
    Token { token: String, lifetime: Duration },
    /// The login must not be used. The reason is for the log.
    Refused(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct BearerChallenge {
    pub realm: String,
    pub service: Option<String>,
}

/// What a registry's `WWW-Authenticate` header asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Challenge {
    None,
    Bearer(BearerChallenge),
    /// Basic or unreadable. oci-client sends a Basic login again for every
    /// new token, and would follow a later Bearer challenge with it, so such
    /// a registry is not given the login.
    Unusable(&'static str),
}

/// All `WWW-Authenticate` headers as one challenge list. A header that is
/// not text makes the registry unusable.
fn challenge_from_headers(headers: &reqwest::header::HeaderMap) -> Challenge {
    let mut texts = Vec::new();
    for value in headers.get_all(reqwest::header::WWW_AUTHENTICATE) {
        match value.to_str() {
            Ok(text) => texts.push(text),
            Err(_) => return Challenge::Unusable("the challenge is not text"),
        }
    }
    if texts.is_empty() {
        Challenge::None
    } else {
        parse_challenge(&texts.join(", "))
    }
}

/// Read the first Bearer challenge with the parser oci-client uses, and fail
/// closed on anything odd.
fn parse_challenge(text: &str) -> Challenge {
    let mut saw_basic = false;
    let mut seen_any = false;
    for parsed in ChallengeParser::new(text) {
        let Ok(challenge) = parsed else {
            return Challenge::Unusable("the challenge cannot be parsed");
        };
        seen_any = true;
        if challenge.scheme.eq_ignore_ascii_case(BASIC_SCHEME) {
            saw_basic = true;
        }
        if !challenge.scheme.eq_ignore_ascii_case(BEARER_SCHEME) {
            continue;
        }
        let (mut realm, mut service) = (None, None);
        for (key, value) in &challenge.params {
            let slot = if key.eq_ignore_ascii_case("realm") {
                &mut realm
            } else if key.eq_ignore_ascii_case("service") {
                &mut service
            } else {
                continue;
            };
            if slot.replace(value.to_unescaped()).is_some() {
                return Challenge::Unusable("the challenge repeats a parameter");
            }
        }
        if let Some(realm) = realm {
            return Challenge::Bearer(BearerChallenge { realm, service });
        }
    }
    if saw_basic {
        Challenge::Unusable("the registry asks for Basic authentication, which is not supported")
    } else if seen_any {
        Challenge::Unusable("the registry does not use a token service")
    } else {
        Challenge::Unusable("the challenge is empty")
    }
}

/// The realm as a URL, if a login may be sent to it: https, or plain HTTP
/// on the host of a registry the operator configured as plain HTTP.
pub(super) fn acceptable_realm(realm: &str, registry: &str, registry_is_http: bool) -> Option<Url> {
    // The parsed scheme is what counts: the URL parser drops tabs and
    // newlines, and the header's quoted-string escapes are already undone.
    let url = Url::parse(realm).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.scheme() {
        HTTPS_SCHEME => Some(url),
        HTTP_SCHEME if registry_is_http => {
            let registry_url = Url::parse(&format!("{HTTP_SCHEME}://{registry}/")).ok()?;
            (url.host_str() == registry_url.host_str()
                && url.port_or_known_default() == registry_url.port_or_known_default())
            .then_some(url)
        }
        _ => None,
    }
}

/// A client for the login requests. It never follows a redirect, so a login
/// cannot be passed on to another host.
fn login_client(timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(REGISTRY_CONNECT_TIMEOUT)
        .read_timeout(REGISTRY_READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Ask the registry what it wants, and when it wants a token service, get a
/// token for `repository` from it with `credentials`.
///
/// A redirect from `/v2/` means no login is used: oci-client would follow it
/// and read a challenge from a host Ward has not judged.
pub(super) async fn plan_login(
    registry: &str,
    registry_is_http: bool,
    repository: &str,
    credentials: &Credentials,
    timeout: Duration,
) -> Result<LoginPlan, OciDistributionError> {
    let client = login_client(timeout)?;
    let scheme = if registry_is_http {
        HTTP_SCHEME
    } else {
        HTTPS_SCHEME
    };
    let response = client
        .get(format!("{scheme}://{registry}/v2/"))
        .send()
        .await?;
    if response.status().is_redirection() {
        return Ok(LoginPlan::Refused(
            "the registry redirects its /v2/ endpoint",
        ));
    }
    let challenge = challenge_from_headers(response.headers());
    let status = response.status();
    // An answer that is neither success nor a challenge says nothing about
    // whether a login is needed, so it is an error and nothing is cached.
    if challenge == Challenge::None
        && !status.is_success()
        && status != reqwest::StatusCode::UNAUTHORIZED
    {
        return Err(token_status_error(status));
    }
    let bearer = match challenge {
        Challenge::None => return Ok(LoginPlan::NoLoginNeeded),
        Challenge::Unusable(reason) => return Ok(LoginPlan::Refused(reason)),
        Challenge::Bearer(bearer) => bearer,
    };
    let Some(realm) = acceptable_realm(&bearer.realm, registry, registry_is_http) else {
        return Ok(LoginPlan::Refused("the token service is not https"));
    };
    let mut query = vec![("scope", format!("repository:{repository}:pull"))];
    if let Some(service) = bearer.service {
        query.push(("service", service));
    }
    let mut response = client
        .get(realm)
        .query(&query)
        .basic_auth(&credentials.username, Some(&credentials.password))
        .send()
        .await?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(token_status_error(response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_TOKEN_RESPONSE_BYTES {
            return Err(OciDistributionError::RegistryTokenDecodeError(
                "the token response is too large".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    parse_token_response(&body)
}

/// The error for a status a registry or token service answered with. 401 and
/// 403 mean the credentials were refused. Anything else says nothing about
/// them: 429 and 5xx are retried, and the rest fail the call.
fn token_status_error(status: reqwest::StatusCode) -> OciDistributionError {
    match status.as_u16() {
        401 | 403 => OciDistributionError::AuthenticationFailure(
            "the token service refused the login".to_string(),
        ),
        code => OciDistributionError::ServerError {
            code,
            url: String::new(),
            message: "unexpected answer while logging in".to_string(),
        },
    }
}

fn parse_token_response(body: &[u8]) -> Result<LoginPlan, OciDistributionError> {
    let invalid = || {
        OciDistributionError::RegistryTokenDecodeError("the token response is invalid".to_string())
    };
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let token = ["token", "access_token"]
        .iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .filter(|token| !token.is_empty())
        // oci-client unwraps this when it builds the Authorization header.
        .filter(|token| reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_ok())
        .ok_or_else(invalid)?;
    let lifetime = value
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_TOKEN_LIFETIME, Duration::from_secs)
        .clamp(MIN_TOKEN_LIFETIME, MAX_TOKEN_LIFETIME);
    Ok(LoginPlan::Token {
        token: token.to_string(),
        lifetime,
    })
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const TEST_LOGIN_TIMEOUT: Duration = Duration::from_millis(200);
    const DRIP_INTERVAL: Duration = Duration::from_millis(20);
    const DRIP_BYTES: usize = 250;

    fn challenge(text: &str) -> Challenge {
        parse_challenge(text)
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(
                WWW_AUTHENTICATE,
                HeaderValue::from_str(value).expect("header"),
            );
        }
        map
    }

    #[test]
    fn given_a_bearer_header_when_parsed_then_the_realm_is_unescaped() {
        // Act
        let parsed = challenge(r#"Bearer realm="\h\t\t\p://auth.example/token",service="reg""#);

        // Assert
        assert_eq!(
            parsed,
            Challenge::Bearer(BearerChallenge {
                realm: "http://auth.example/token".to_string(),
                service: Some("reg".to_string()),
            })
        );
    }

    #[test]
    fn given_a_basic_or_odd_header_when_parsed_then_it_is_unusable() {
        // Assert
        assert!(matches!(
            challenge(r#"Basic realm="registry""#),
            Challenge::Unusable(_)
        ));
        assert!(matches!(challenge(""), Challenge::Unusable(_)));
        assert!(matches!(challenge("Bearer"), Challenge::Unusable(_)));
        assert_eq!(challenge_from_headers(&HeaderMap::new()), Challenge::None);
    }

    #[test]
    fn given_a_basic_challenge_when_parsed_then_the_reason_names_basic() {
        // Act
        let parsed = challenge(r#"Basic realm="registry""#);

        // Assert
        assert!(
            matches!(parsed, Challenge::Unusable(reason) if reason.contains("Basic")),
            "{parsed:?}"
        );
    }

    #[test]
    fn given_a_repeated_realm_when_parsed_then_the_challenge_is_unusable() {
        // Act
        let parsed = challenge(r#"Bearer realm="https://a.example/t",realm="http://b.example/t""#);

        // Assert
        assert!(matches!(parsed, Challenge::Unusable(_)), "{parsed:?}");
    }

    #[test]
    fn given_several_challenge_headers_when_read_then_the_first_bearer_one_is_used() {
        // Act
        let parsed = challenge_from_headers(&headers(&[
            r#"Basic realm="registry""#,
            r#"Bearer realm="https://auth.example/token",service="reg""#,
        ]));

        // Assert
        assert!(
            matches!(parsed, Challenge::Bearer(ref b) if b.realm == "https://auth.example/token"),
            "{parsed:?}"
        );
    }

    #[test]
    fn given_a_bad_bearer_header_before_a_good_one_when_read_then_the_bad_realm_is_judged() {
        // Act
        let parsed = challenge_from_headers(&headers(&[
            r#"Bearer realm="http://other.example/token""#,
            r#"Bearer realm="https://auth.example/token""#,
        ]));

        // Assert
        assert!(
            matches!(parsed, Challenge::Bearer(ref b) if b.realm == "http://other.example/token"),
            "{parsed:?}"
        );
        assert!(
            acceptable_realm("http://other.example/token", "registry.example", false).is_none()
        );
    }

    #[test]
    fn given_statuses_when_mapped_then_401_and_403_refuse_the_login_and_the_rest_do_not() {
        // Assert
        for code in [401, 403] {
            let status = reqwest::StatusCode::from_u16(code).expect("status");
            assert!(matches!(
                token_status_error(status),
                OciDistributionError::AuthenticationFailure(_)
            ));
        }
        for code in [429, 500, 503, 404] {
            let status = reqwest::StatusCode::from_u16(code).expect("status");
            assert!(matches!(
                token_status_error(status),
                OciDistributionError::ServerError { code: got, .. } if got == code
            ));
        }
    }

    #[test]
    fn given_a_tiny_expires_in_when_parsed_then_the_lifetime_is_raised_to_the_floor() {
        // Assert
        assert!(matches!(
            parse_token_response(br#"{"token":"t","expires_in":1}"#),
            Ok(LoginPlan::Token { lifetime, .. }) if lifetime == MIN_TOKEN_LIFETIME
        ));
    }

    /// A registry on loopback whose token service sends `body_bytes` of body,
    /// one byte every `DRIP_INTERVAL` when `slowly`.
    async fn registry_with_token_body(body_bytes: usize, slowly: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    let read = stream.read(&mut request).await.unwrap_or(0);
                    let line = String::from_utf8_lossy(&request[..read]).into_owned();
                    let head = if line.starts_with("GET /v2/") {
                        format!(
                            "HTTP/1.1 401 X\r\nWWW-Authenticate: Bearer realm=\"http://{addr}/token\"\r\n\
                             Content-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 X\r\nContent-Length: {body_bytes}\r\n\
                             Connection: close\r\n\r\n"
                        )
                    };
                    let _ = stream.write_all(head.as_bytes()).await;
                    if line.starts_with("GET /v2/") {
                        return;
                    }
                    for _ in 0..body_bytes {
                        if stream.write_all(b" ").await.is_err() {
                            return;
                        }
                        if slowly {
                            tokio::time::sleep(DRIP_INTERVAL).await;
                        }
                    }
                });
            }
        });
        addr
    }

    fn credentials() -> Credentials {
        Credentials {
            username: "ada".to_string(),
            password: "pw".to_string(),
        }
    }

    #[tokio::test]
    async fn given_a_token_body_over_the_cap_when_the_login_runs_then_it_fails() {
        // Arrange
        let addr = registry_with_token_body(MAX_TOKEN_RESPONSE_BYTES + 1024, false).await;

        // Act
        let outcome = plan_login(
            &addr.to_string(),
            true,
            "team/app",
            &credentials(),
            LOGIN_REQUEST_TIMEOUT,
        )
        .await;

        // Assert
        assert!(
            matches!(
                outcome,
                Err(OciDistributionError::RegistryTokenDecodeError(_))
            ),
            "{}",
            outcome.is_ok()
        );
    }

    #[tokio::test]
    async fn given_a_token_service_that_drips_when_the_login_runs_then_the_total_timeout_ends_it() {
        // Arrange
        let addr = registry_with_token_body(DRIP_BYTES, true).await;
        let started = std::time::Instant::now();

        // Act
        let outcome = plan_login(
            &addr.to_string(),
            true,
            "team/app",
            &credentials(),
            TEST_LOGIN_TIMEOUT,
        )
        .await;

        // Assert
        assert!(
            matches!(&outcome, Err(OciDistributionError::RequestError(e)) if e.is_timeout()),
            "{}",
            outcome.is_ok()
        );
        assert!(started.elapsed() < DRIP_INTERVAL * DRIP_BYTES as u32);
    }

    #[test]
    fn given_realms_when_judged_then_only_https_or_the_configured_host_pass() {
        // Assert
        let registry = "127.0.0.1:5000";
        assert!(
            acceptable_realm("https://auth.example/token", "registry.example", false).is_some()
        );
        assert!(acceptable_realm("http://auth.example/token", "registry.example", false).is_none());
        assert!(acceptable_realm("HTTP://auth.example/token", "registry.example", false).is_none());
        assert!(
            acceptable_realm("ht\ttp://auth.example/token", "registry.example", false).is_none()
        );
        assert!(acceptable_realm("not a url", "registry.example", false).is_none());
        assert!(
            acceptable_realm("https://u:p@auth.example/t", "registry.example", false).is_none()
        );
        assert!(acceptable_realm("http://127.0.0.1:5000/token", registry, true).is_some());
        assert!(acceptable_realm("http://127.0.0.1:5001/token", registry, true).is_none());
        assert!(acceptable_realm("http://other.example:5000/token", registry, true).is_none());
        assert!(acceptable_realm("http://127.0.0.1:5000/token", registry, false).is_none());
    }

    #[test]
    fn given_a_token_response_when_parsed_then_token_and_lifetime_are_read() {
        // Act
        let plan = parse_token_response(br#"{"access_token":"abc","expires_in":300}"#);

        // Assert
        assert!(matches!(
            plan,
            Ok(LoginPlan::Token { ref token, lifetime })
                if token == "abc" && lifetime == Duration::from_secs(300)
        ));
        assert!(matches!(
            parse_token_response(br#"{"token":"t"}"#),
            Ok(LoginPlan::Token { lifetime, .. }) if lifetime == DEFAULT_TOKEN_LIFETIME
        ));
        assert!(matches!(
            parse_token_response(br#"{"token":"t","expires_in":99999999}"#),
            Ok(LoginPlan::Token { lifetime, .. }) if lifetime == MAX_TOKEN_LIFETIME
        ));
        assert!(parse_token_response(br#"{"token":""}"#).is_err());
        assert!(parse_token_response(b"{\"token\":\"a\\nb\"}").is_err());
        assert!(parse_token_response(b"not json").is_err());
    }
}
