// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Registry credentials in Docker's `config.json`, so `ward login` and an
//! existing `docker login` share one file.
//!
//! Only the `auths` map is read and written. Credential helpers are not
//! supported. Nothing here logs or prints a secret.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

const CONFIG_DIR_ENV: &str = "DOCKER_CONFIG";
const HOME_ENV: &str = "HOME";
const DEFAULT_DIR: &str = ".docker";
const CONFIG_FILE: &str = "config.json";
const AUTHS_KEY: &str = "auths";
const AUTH_KEY: &str = "auth";
const USERNAME_KEY: &str = "username";
const PASSWORD_KEY: &str = "password";
const HELPER_KEYS: [&str; 2] = ["credsStore", "credHelpers"];
const OWNER_ONLY: u32 = 0o600;
const DIR_MODE: u32 = 0o700;
/// The key Docker uses for Docker Hub.
pub const DOCKER_HUB_SERVER: &str = "https://index.docker.io/v1/";
const DOCKER_HUB_HOSTS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];
const DOCKER_HUB_HOST: &str = "docker.io";

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<hidden>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("cannot use the credentials file {0}: {1}")]
    File(PathBuf, String),
    #[error("credential helpers are not supported; remove credsStore and credHelpers from {0}")]
    Helper(PathBuf),
}

/// `$DOCKER_CONFIG/config.json`, or `~/.docker/config.json`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join(CONFIG_FILE));
    }
    std::env::var_os(HOME_ENV)
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(DEFAULT_DIR).join(CONFIG_FILE))
}

/// The registry host in `server`, which may be a URL as Docker stores it.
/// Docker Hub's names all map to `docker.io`.
pub fn registry_host(server: &str) -> String {
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    let host = rest.split('/').next().unwrap_or(rest);
    if DOCKER_HUB_HOSTS.contains(&host) {
        DOCKER_HUB_HOST.to_string()
    } else {
        host.to_string()
    }
}

/// The `auths` key to store `server` under.
fn storage_key(server: &str) -> String {
    let host = registry_host(server);
    if host == DOCKER_HUB_HOST {
        DOCKER_HUB_SERVER.to_string()
    } else {
        host
    }
}

fn read_config(path: &Path) -> Result<Map<String, Value>, AuthError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(AuthError::File(path.to_path_buf(), e.to_string())),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(AuthError::File(
            path.to_path_buf(),
            "not a JSON object".to_string(),
        )),
    }
}

fn decode(entry: &Value) -> Option<Credentials> {
    if let Some(encoded) = entry[AUTH_KEY].as_str().filter(|a| !a.is_empty()) {
        let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
        let (username, password) = decoded.split_once(':')?;
        return Some(Credentials {
            username: username.to_string(),
            password: password.to_string(),
        });
    }
    Some(Credentials {
        username: entry[USERNAME_KEY].as_str()?.to_string(),
        password: entry[PASSWORD_KEY].as_str()?.to_string(),
    })
}

/// The stored credentials for the registry `server` names, if any. A file
/// that cannot be read or has no entry gives `None`, so a pull falls back to
/// anonymous access.
pub fn lookup(server: &str) -> Option<Credentials> {
    let config = read_config(&config_path()?).ok()?;
    let wanted = registry_host(server);
    config
        .get(AUTHS_KEY)?
        .as_object()?
        .iter()
        .find(|(key, _)| registry_host(key) == wanted)
        .and_then(|(_, entry)| decode(entry))
}

fn write_config(path: &Path, config: Map<String, Value>) -> Result<(), AuthError> {
    let fail = |e: std::io::Error| AuthError::File(path.to_path_buf(), e.to_string());
    if let Some(dir) = path.parent().filter(|dir| !dir.exists()) {
        std::fs::create_dir_all(dir).map_err(fail)?;
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(DIR_MODE))
            .map_err(fail)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(&Value::Object(config)).map_err(|e| fail(e.into()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(OWNER_ONLY)
        .open(&temp)
        .map_err(fail)?;
    file.write_all(&body)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
        .map_err(fail)
}

fn refuse_helpers(config: &Map<String, Value>, path: &Path) -> Result<(), AuthError> {
    if HELPER_KEYS.iter().any(|key| config.contains_key(*key)) {
        return Err(AuthError::Helper(path.to_path_buf()));
    }
    Ok(())
}

/// Saves `credentials` for `server`, keeping every other setting in the file.
pub fn store(server: &str, credentials: &Credentials) -> Result<(), AuthError> {
    let path = config_path()
        .ok_or_else(|| AuthError::File(PathBuf::new(), "no home directory".to_string()))?;
    let mut config = read_config(&path)?;
    refuse_helpers(&config, &path)?;
    let auth = STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
    let auths = config
        .entry(AUTHS_KEY)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AuthError::File(path.to_path_buf(), "auths is not an object".to_string()))?;
    let key = auths
        .keys()
        .find(|key| registry_host(key) == registry_host(server))
        .cloned()
        .unwrap_or_else(|| storage_key(server));
    auths.insert(key, json!({ AUTH_KEY: auth }));
    write_config(&path, config)
}

/// Removes the entry for `server`. Returns whether there was one.
pub fn erase(server: &str) -> Result<bool, AuthError> {
    let Some(path) = config_path() else {
        return Ok(false);
    };
    let mut config = read_config(&path)?;
    let Some(auths) = config.get_mut(AUTHS_KEY).and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let before = auths.len();
    auths.retain(|key, _| registry_host(key) != registry_host(server));
    if auths.len() == before {
        return Ok(false);
    }
    write_config(&path, config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Tests share the process environment, so they run one after another.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_config_dir<T>(body: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: the lock above serialises every test that touches the variable.
        unsafe { std::env::set_var(CONFIG_DIR_ENV, dir.path()) };
        let out = body(dir.path());
        unsafe { std::env::remove_var(CONFIG_DIR_ENV) };
        out
    }

    fn creds() -> Credentials {
        Credentials {
            username: "ada".to_string(),
            password: "s3cret:with:colons".to_string(),
        }
    }

    #[test]
    fn given_stored_credentials_when_looked_up_then_hub_aliases_and_ports_match() {
        with_config_dir(|_| {
            store("docker.io", &creds()).expect("store");

            assert_eq!(lookup("index.docker.io"), Some(creds()));
            assert_eq!(lookup("registry.example.com"), None);
            store("localhost:5000", &creds()).expect("store");
            assert_eq!(lookup("localhost:5000"), Some(creds()));
        });
    }

    #[test]
    fn given_an_existing_config_when_stored_then_other_keys_stay_and_mode_is_private() {
        with_config_dir(|dir| {
            let path = dir.join(CONFIG_FILE);
            std::fs::write(
                &path,
                r#"{"currentContext":"x","auths":{"a.io":{"auth":"eDp5"}}}"#,
            )
            .expect("seed");

            store("b.io", &creds()).expect("store");

            let saved: Value =
                serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
            assert_eq!(saved["currentContext"], "x");
            assert!(saved["auths"]["a.io"].is_object());
            assert!(saved["auths"]["b.io"]["auth"].is_string());
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, OWNER_ONLY);
        });
    }

    #[test]
    fn given_a_stored_login_when_erased_then_it_is_gone_and_a_second_erase_says_so() {
        with_config_dir(|_| {
            store("b.io", &creds()).expect("store");

            assert!(erase("b.io").expect("erase"));
            assert!(!erase("b.io").expect("erase again"));
            assert_eq!(lookup("b.io"), None);
        });
    }

    #[test]
    fn given_a_credential_helper_when_stored_then_it_is_refused() {
        with_config_dir(|dir| {
            std::fs::write(dir.join(CONFIG_FILE), r#"{"credsStore":"osxkeychain"}"#).expect("seed");

            let error = store("b.io", &creds()).expect_err("refused");

            assert!(matches!(error, AuthError::Helper(_)));
        });
    }

    #[test]
    fn given_credentials_when_debug_printed_then_the_password_is_hidden() {
        assert!(!format!("{:?}", creds()).contains("s3cret"));
    }
}
