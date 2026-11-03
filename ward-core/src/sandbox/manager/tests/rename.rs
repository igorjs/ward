// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Renaming a sandbox through the manager.

use pretty_assertions::assert_eq;

use super::*;

const OLD_NAME: &str = "web1";
const NEW_NAME: &str = "web2";
const OTHER_NAME: &str = "cache";
const MAX_MANAGER_SANDBOXES: usize = 4;
const EVENT_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

#[tokio::test]
async fn given_named_sandbox_when_renamed_then_new_name_resolves_and_old_does_not() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(named(OLD_NAME), "").await.unwrap();

    // Act
    let renamed = mgr.rename(OLD_NAME, NEW_NAME).await.unwrap();

    // Assert
    assert_eq!(renamed.name, NEW_NAME);
    assert_eq!(mgr.resolve(NEW_NAME).await.unwrap(), created.id);
    assert!(matches!(
        mgr.resolve(OLD_NAME).await,
        Err(ApiError::SandboxNotFound(_))
    ));
    assert_eq!(mgr.list().await.unwrap()[0].name, NEW_NAME);
}

#[tokio::test]
async fn given_renamed_sandbox_when_old_name_reused_then_create_succeeds() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(named(OLD_NAME), "").await.unwrap();
    mgr.rename(OLD_NAME, NEW_NAME).await.unwrap();

    // Act
    let again = mgr.create(named(OLD_NAME), "").await;

    // Assert
    assert!(again.is_ok());
}

#[tokio::test]
async fn given_name_held_by_another_when_renamed_then_name_in_use_and_nothing_changes() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(named(OLD_NAME), "").await.unwrap();
    let other = mgr.create(named(OTHER_NAME), "").await.unwrap();

    // Act
    let err = mgr.rename(OLD_NAME, OTHER_NAME).await.expect_err("taken");

    // Assert
    assert!(
        matches!(&err, ApiError::NameInUse { name, id } if name == OTHER_NAME && id.as_deref() == Some(other.id.as_str()))
    );
    assert!(mgr.resolve(OLD_NAME).await.is_ok());
}

#[tokio::test]
async fn given_current_name_when_renamed_to_it_then_rejected() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(named(OLD_NAME), "").await.unwrap();

    // Act
    let err = mgr.rename(OLD_NAME, OLD_NAME).await.expect_err("same");

    // Assert
    assert!(matches!(err, ApiError::InvalidRequest(_)));
}

#[rstest::rstest]
#[case::empty("")]
#[case::leading_dash("-web")]
#[case::space("a b")]
#[tokio::test]
async fn given_invalid_name_when_renamed_then_rejected(#[case] name: &str) {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(named(OLD_NAME), "").await.unwrap();

    // Act
    let err = mgr.rename(OLD_NAME, name).await.expect_err("invalid");

    // Assert
    assert!(matches!(err, ApiError::InvalidRequest(_)));
}

#[tokio::test]
async fn given_unknown_sandbox_when_renamed_then_not_found() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);

    // Act
    let err = mgr.rename("ghost", NEW_NAME).await.expect_err("missing");

    // Assert
    assert!(matches!(err, ApiError::SandboxNotFound(_)));
}

#[tokio::test]
async fn given_renamed_sandbox_when_events_read_then_rename_is_announced() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(named(OLD_NAME), "").await.unwrap();
    let mut reader = mgr.events().subscribe(Some(std::time::UNIX_EPOCH));

    // Act
    mgr.rename(OLD_NAME, NEW_NAME).await.unwrap();

    // Assert
    let mut last = None;
    while let Ok(Some(next)) = tokio::time::timeout(EVENT_WAIT, reader.next()).await {
        last = Some((next.event.action, next.event.name));
    }
    assert_eq!(last, Some((EventAction::Rename, NEW_NAME.to_string())));
}

#[tokio::test]
async fn given_renamed_sandbox_when_daemon_restarts_then_new_name_is_kept() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let first = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = first.create(named(OLD_NAME), "").await.unwrap();
    first.rename(OLD_NAME, NEW_NAME).await.unwrap();

    // Act
    let second = restart(tmp.path()).await;

    // Assert
    assert_eq!(second.resolve(NEW_NAME).await.unwrap(), created.id);
}
