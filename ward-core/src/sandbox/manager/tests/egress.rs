// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Adding hosts to a sandbox's egress allowlist through the manager.

use pretty_assertions::assert_eq;

use super::*;
use crate::pb::{EgressMode as PbMode, EgressPolicy as PbPolicy};

const FIRST_HOST: &str = "pypi.org";
const SECOND_HOST: &str = "files.pythonhosted.org";
const MAX_MANAGER_SANDBOXES: usize = 4;

fn request(mode: PbMode, domains: &[&str]) -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: "alpine".into(),
        egress: Some(PbPolicy {
            mode: mode as i32,
            domains: domains.iter().map(ToString::to_string).collect(),
        }),
        ..Default::default()
    }
}

fn hosts(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

async fn stored_domains(mgr: &SandboxManager, id: &str) -> Vec<String> {
    let entries = mgr.entries.read().await;
    entries[id].meta.options.egress.domains.clone()
}

#[tokio::test]
async fn given_allowlist_sandbox_when_host_added_then_reported_stored_and_announced() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    let mut reader = mgr.events().subscribe(Some(std::time::UNIX_EPOCH));

    // Act
    let added = mgr
        .add_egress_allow(&created.id, hosts(&[FIRST_HOST, SECOND_HOST, SECOND_HOST]))
        .await
        .unwrap();

    // Assert
    assert_eq!(added, [SECOND_HOST]);
    assert_eq!(
        stored_domains(&mgr, &created.id).await,
        [FIRST_HOST, SECOND_HOST]
    );
    let mut actions = Vec::new();
    while let Ok(Some(next)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), reader.next()).await
    {
        actions.push(next.event.action);
    }
    assert_eq!(actions.last(), Some(&EventAction::Update));
}

#[tokio::test]
async fn given_added_host_when_daemon_restarts_then_allowlist_keeps_it() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let first = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = first
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    first
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .unwrap();

    // Act
    let second = restart(tmp.path()).await;

    // Assert
    assert_eq!(
        stored_domains(&second, &created.id).await,
        [FIRST_HOST, SECOND_HOST]
    );
    let again = second
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .unwrap();
    assert!(again.is_empty(), "the reloaded backend still lists it");
}

#[tokio::test]
async fn given_deny_sandbox_when_host_added_then_refused_with_recreate_hint() {
    // Arrange
    let mgr = manager_on(tempfile::tempdir().unwrap().path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(request(PbMode::Deny, &[]), "").await.unwrap();

    // Act
    let error = mgr
        .add_egress_allow(&created.id, hosts(&[FIRST_HOST]))
        .await
        .expect_err("deny mode refuses");

    // Assert
    match error {
        ApiError::InvalidRequest(msg) => assert_eq!(
            msg,
            "sandbox was created with egress deny; recreate it with --egress allowlist"
        ),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn given_open_sandbox_when_host_added_then_nothing_changes() {
    // Arrange
    let mgr = manager_on(tempfile::tempdir().unwrap().path(), MAX_MANAGER_SANDBOXES);
    let created = mgr.create(request(PbMode::Open, &[]), "").await.unwrap();

    // Act
    let added = mgr
        .add_egress_allow(&created.id, hosts(&[FIRST_HOST]))
        .await
        .unwrap();

    // Assert
    assert!(added.is_empty());
    assert!(stored_domains(&mgr, &created.id).await.is_empty());
}

#[tokio::test]
async fn given_invalid_or_too_many_hosts_when_added_then_refused_and_list_unchanged() {
    // Arrange
    let mgr = manager_on(tempfile::tempdir().unwrap().path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    let too_many: Vec<String> = (0..crate::validate::MAX_EGRESS_DOMAINS)
        .map(|i| format!("h{i}.example"))
        .collect();

    // Act
    let bad_syntax = mgr
        .add_egress_allow(&created.id, hosts(&["not a host"]))
        .await;
    let over_cap = mgr.add_egress_allow(&created.id, too_many).await;

    // Assert
    assert!(matches!(bad_syntax, Err(ApiError::InvalidRequest(_))));
    assert!(matches!(over_cap, Err(ApiError::InvalidRequest(_))));
    assert_eq!(stored_domains(&mgr, &created.id).await, [FIRST_HOST]);
}

const STALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

async fn within<T>(what: &str, work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STALL_DEADLINE, work)
        .await
        .unwrap_or_else(|_| panic!("{what} did not finish"))
}

#[tokio::test]
async fn given_backend_stuck_on_egress_update_when_listing_and_inspecting_then_both_answer() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    gate.block.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        let id = created.id.clone();
        async move { mgr.add_egress_allow(&id, hosts(&[SECOND_HOST])).await }
    });
    within("the backend call starting", gate.entered.notified()).await;

    // Act
    let listed = within("list", mgr.list()).await.unwrap();
    let inspected = within("inspect", mgr.get(&created.id)).await.unwrap();
    gate.release.notify_one();
    let added = within("add_egress_allow", pending).await.unwrap().unwrap();

    // Assert
    assert_eq!(listed.len(), 1);
    assert_eq!(inspected.id, created.id);
    assert_eq!(added, [SECOND_HOST]);
}

#[tokio::test]
async fn given_backend_refuses_egress_update_when_host_added_then_record_keeps_old_list() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    gate.fail.store(true, Ordering::SeqCst);

    // Act
    let result = mgr
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await;

    // Assert
    assert!(result.is_err());
    assert_eq!(stored_domains(&mgr, &created.id).await, [FIRST_HOST]);
    let second = restart(tmp.path()).await;
    assert_eq!(stored_domains(&second, &created.id).await, [FIRST_HOST]);
}

#[tokio::test]
async fn given_sandbox_removed_during_egress_update_when_backend_returns_then_call_fails_cleanly() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    gate.block.store(true, Ordering::SeqCst);
    let pending = tokio::spawn({
        let mgr = Arc::clone(&mgr);
        let id = created.id.clone();
        async move { mgr.add_egress_allow(&id, hosts(&[SECOND_HOST])).await }
    });
    within("the backend call starting", gate.entered.notified()).await;

    // Act
    within("remove", mgr.remove(&created.id)).await.unwrap();
    gate.release.notify_one();
    let result = within("add_egress_allow", pending).await.unwrap();

    // Assert
    assert!(result.is_err());
    assert!(mgr.list().await.unwrap().is_empty());
    let second = restart(tmp.path()).await;
    assert!(second.list().await.unwrap().is_empty());
}

fn seen(gate: &EgressGate) -> Vec<Vec<String>> {
    gate.seen.lock().unwrap().clone()
}

#[tokio::test]
async fn given_first_call_refused_when_retried_then_backend_gets_host_and_record_matches() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    gate.fail.store(true, Ordering::SeqCst);
    mgr.add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .expect_err("backend refuses");
    gate.fail.store(false, Ordering::SeqCst);

    // Act
    let added = mgr
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .unwrap();

    // Assert
    assert_eq!(added, [SECOND_HOST]);
    assert_eq!(seen(&gate), [hosts(&[SECOND_HOST]), hosts(&[SECOND_HOST])]);
    assert_eq!(
        stored_domains(&mgr, &created.id).await,
        [FIRST_HOST, SECOND_HOST]
    );
}

#[tokio::test]
async fn given_host_recorded_but_not_in_backend_when_added_again_then_backend_gets_it_quietly() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    // The state a dropped call leaves: recorded, never sent.
    mgr.entries
        .write()
        .await
        .get_mut(&created.id)
        .unwrap()
        .meta
        .options
        .egress
        .domains
        .push(SECOND_HOST.to_string());
    let mut reader = mgr.events().subscribe(Some(std::time::SystemTime::now()));

    // Act
    let added = mgr
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .unwrap();

    // Assert
    assert!(added.is_empty());
    assert_eq!(seen(&gate), [hosts(&[SECOND_HOST])]);
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), reader.next()).await;
    assert!(event.is_err(), "a repair announces nothing");
}

#[tokio::test]
async fn given_rollback_cannot_be_saved_when_backend_refuses_then_retry_repairs_the_vm() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _, gate) = manager_with_gates(tmp.path(), MAX_MANAGER_SANDBOXES);
    let created = mgr
        .create(request(PbMode::Allowlist, &[FIRST_HOST]), "")
        .await
        .unwrap();
    let blocker = tmp
        .path()
        .join("sandboxes")
        .join(&created.id)
        .join(crate::sandbox::store::META_TMP_FILE);
    *gate.break_store_at.lock().unwrap() = Some(blocker.clone());
    gate.fail.store(true, Ordering::SeqCst);

    // Act
    let refused = mgr
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await;
    std::fs::remove_dir(&blocker).unwrap();
    gate.fail.store(false, Ordering::SeqCst);
    let retried = mgr
        .add_egress_allow(&created.id, hosts(&[SECOND_HOST]))
        .await
        .unwrap();

    // Assert
    assert!(refused.is_err());
    assert!(retried.is_empty(), "the host was still recorded");
    assert_eq!(seen(&gate), [hosts(&[SECOND_HOST]), hosts(&[SECOND_HOST])]);
    assert_eq!(
        stored_domains(&mgr, &created.id).await,
        [FIRST_HOST, SECOND_HOST]
    );
}
