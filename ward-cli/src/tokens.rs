// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Local capability-token store for the ward CLI.
//!
//! Every `ward create` persists the sandbox's capability token to a
//! per-user JSON file alongside the daemon socket, so a later `ward
//! <subcommand> <sandbox_id>` invocation in a fresh process can read it
//! back and attach it to the gRPC request without the caller having to
//! pass it explicitly.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Build the token store's path as a sibling of the daemon's socket file.
pub fn store_path(socket_path: &str) -> PathBuf {
    Path::new(socket_path)
        .parent()
        .unwrap_or(Path::new("."))
        .join("tokens.json")
}

/// Load the whole store as a sandbox_id -> token map.
///
/// A missing file is a fresh store, not an error. A present-but-corrupt
/// file is a real error and gets propagated rather than discarded, since
/// silently treating it as empty would lose every previously saved token.
fn read_map(store_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    if !store_path.exists() {
        return Ok(HashMap::new());
    }
    let contents = std::fs::read_to_string(store_path)?;
    let map = serde_json::from_str(&contents)?;
    Ok(map)
}

fn write_map(store_path: &Path, map: &HashMap<String, String>) -> anyhow::Result<()> {
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string(map)?;
    std::fs::write(store_path, contents)?;
    std::fs::set_permissions(store_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Persist `token` for `sandbox_id`, merging into any existing store.
pub fn save(store_path: &Path, sandbox_id: &str, token: &str) -> anyhow::Result<()> {
    let mut map = read_map(store_path)?;
    map.insert(sandbox_id.to_string(), token.to_string());
    write_map(store_path, &map)
}

/// Look up the stored token for `sandbox_id`, if any.
///
/// Both a missing store file and a missing key are normal (a fresh daemon,
/// or a sandbox created before this change), not errors.
pub fn load(store_path: &Path, sandbox_id: &str) -> anyhow::Result<Option<String>> {
    let map = read_map(store_path)?;
    Ok(map.get(sandbox_id).cloned())
}

/// Drop `sandbox_id`'s entry from the store, if present.
///
/// A no-op (not an error) when the store or the entry doesn't exist, since
/// removing a token that was never saved is not a failure condition.
pub fn remove(store_path: &Path, sandbox_id: &str) -> anyhow::Result<()> {
    if !store_path.exists() {
        return Ok(());
    }
    let mut map = read_map(store_path)?;
    if map.remove(sandbox_id).is_none() {
        return Ok(());
    }
    write_map(store_path, &map)
}

/// Attach the sandbox's capability token to an outgoing gRPC request.
///
/// A no-op when `token` is `None`, matching call sites for sandboxes that
/// predate this change and have no stored token.
pub fn attach_token<T>(request: &mut tonic::Request<T>, token: Option<String>) {
    if let Some(t) = token {
        request
            .metadata_mut()
            .insert("x-ward-sandbox-token", t.parse().expect("valid token"));
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// BDD/AAA style: function names read as `given_X_when_Y_then_Z`, bodies
// have explicit Arrange / Act / Assert markers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn given_saved_token_when_loaded_then_returns_same_value() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("tokens.json");

        // Act
        save(&store, "sandbox-1", "token-abc").expect("save");
        let loaded = load(&store, "sandbox-1").expect("load");

        // Assert
        assert_eq!(loaded, Some("token-abc".to_string()));
    }

    #[test]
    fn given_no_saved_token_when_loaded_then_returns_none() {
        // Arrange: store file does not exist yet
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("tokens.json");

        // Act
        let loaded = load(&store, "sandbox-1").expect("load");

        // Assert
        assert_eq!(loaded, None);
    }

    #[test]
    fn given_store_written_when_permissions_checked_then_mode_is_0600() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("tokens.json");

        // Act
        save(&store, "sandbox-1", "token-abc").expect("save");

        // Assert
        let mode = std::fs::metadata(&store)
            .expect("store metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn given_saved_token_when_removed_then_subsequent_load_returns_none() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("tokens.json");
        save(&store, "sandbox-1", "token-abc").expect("save");

        // Act
        remove(&store, "sandbox-1").expect("remove");
        let loaded = load(&store, "sandbox-1").expect("load");

        // Assert
        assert_eq!(loaded, None);
    }

    #[test]
    fn given_remove_on_empty_store_when_called_then_no_op_ok() {
        // Arrange: no file at the store path, nothing was ever saved
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("tokens.json");

        // Act
        let result = remove(&store, "sandbox-1");

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn given_some_token_when_attach_token_then_metadata_header_set() {
        // Arrange
        let mut request = tonic::Request::new(());

        // Act
        attach_token(&mut request, Some("t".to_string()));

        // Assert
        let value = request
            .metadata()
            .get("x-ward-sandbox-token")
            .expect("header present")
            .to_str()
            .expect("header is valid ascii");
        assert_eq!(value, "t");
    }

    #[test]
    fn given_none_when_attach_token_then_metadata_unchanged() {
        // Arrange
        let mut request = tonic::Request::new(());

        // Act
        attach_token(&mut request, None);

        // Assert
        assert_eq!(request.metadata().len(), 0);
    }

    // Asserts `attach_token` attaches its header regardless of the request's
    // body type, covering every sandbox-scoped request builder the CLI sends
    // without repeating the same arrange/act/assert per type.
    #[test]
    fn given_each_of_13_sandbox_scoped_request_builders_when_attach_token_called_then_header_present()
     {
        fn assert_header_present<T>(mut request: tonic::Request<T>) {
            attach_token(&mut request, Some("t".to_string()));
            let value = request
                .metadata()
                .get("x-ward-sandbox-token")
                .expect("header present")
                .to_str()
                .expect("header is valid ascii");
            assert_eq!(value, "t");
        }

        assert_header_present(tonic::Request::new(ward_core::pb::ExecRequest::default()));
        assert_header_present(tonic::Request::new(ward_core::pb::RunRequest::default()));
        assert_header_present(tonic::Request::new(
            ward_core::pb::StreamOutputRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::WriteStdinRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::KillProcessRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::RemoveSandboxRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::CreateSnapshotRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::RestoreSnapshotRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::ListSnapshotsRequest::default(),
        ));
        assert_header_present(tonic::Request::new(ward_core::pb::PublishRequest::default()));
        assert_header_present(tonic::Request::new(
            ward_core::pb::SubscribeRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::GetEgressLogRequest::default(),
        ));
        assert_header_present(tonic::Request::new(
            ward_core::pb::GetCommunicationLogRequest::default(),
        ));
    }
}
