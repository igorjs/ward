// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Changing a sandbox's limits and restart policy through the manager.

use pretty_assertions::assert_eq;

use super::*;
use crate::pb::{RestartPolicy as PbRestartPolicy, UpdateSandboxRequest};

const NAME: &str = "web1";
const MAX_MANAGER_SANDBOXES: usize = 4;
const VM_CPUS: u32 = 2;
const VM_MEMORY_MB: u32 = 256;
const SMALLER_MEMORY_MB: u32 = 128;
const LARGER_MEMORY_MB: u32 = 512;
const PIDS: u32 = 64;
const HALF_CPU: u64 = 500_000_000;
const THREE_CPUS: u64 = 3_000_000_000;
const EVENT_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

fn sized() -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: "alpine".into(),
        name: NAME.into(),
        resources: Some(PbResourceLimits {
            cpus: VM_CPUS,
            memory_mb: VM_MEMORY_MB,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn change(id: &str) -> UpdateSandboxRequest {
    UpdateSandboxRequest {
        id: id.to_string(),
        ..Default::default()
    }
}

async fn limits_of(mgr: &SandboxManager, id: &str) -> crate::pb::ResourceLimits {
    mgr.get(id).await.unwrap().resources.expect("resources")
}

#[tokio::test]
async fn given_running_sandbox_when_limits_lowered_then_info_reports_them() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;

    // Act
    let updated = mgr
        .update(UpdateSandboxRequest {
            memory_mb: Some(SMALLER_MEMORY_MB),
            pids_max: Some(PIDS),
            nano_cpus: Some(HALF_CPU),
            ..change(NAME)
        })
        .await
        .unwrap();

    // Assert
    let resources = updated.resources.expect("resources");
    assert_eq!(resources.memory_mb, SMALLER_MEMORY_MB);
    assert_eq!(resources.pids_max, PIDS);
    assert_eq!(resources.nano_cpus, HALF_CPU);
    assert_eq!(limits_of(&mgr, &id).await.memory_mb, SMALLER_MEMORY_MB);
}

#[tokio::test]
async fn given_running_sandbox_when_memory_raised_above_the_vm_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            memory_mb: Some(LARGER_MEMORY_MB),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("above the VM's 256 MiB"), "{err}");
    assert_eq!(limits_of(&mgr, &id).await.memory_mb, VM_MEMORY_MB);
}

#[tokio::test]
async fn given_running_sandbox_when_cpus_raised_above_the_vm_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            nano_cpus: Some(THREE_CPUS),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("above the VM's 2 CPUs"), "{err}");
}

#[tokio::test]
async fn given_stopped_sandbox_when_memory_raised_then_the_next_boot_is_sized_to_it() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;
    mgr.stop_within(&id, Some(0)).await.unwrap();

    // Act
    mgr.update(UpdateSandboxRequest {
        memory_mb: Some(LARGER_MEMORY_MB),
        ..change(&id)
    })
    .await
    .unwrap();
    let started = mgr.start(&id).await.unwrap();

    // Assert
    assert_eq!(started.resources.unwrap().memory_mb, LARGER_MEMORY_MB);
    let refused = mgr
        .update(UpdateSandboxRequest {
            memory_mb: Some(LARGER_MEMORY_MB * 2),
            ..change(&id)
        })
        .await;
    assert!(refused.is_err(), "the new VM keeps its size while it runs");
}

#[tokio::test]
async fn given_no_flags_when_updated_then_it_is_refused_like_docker() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let err = mgr.update(change(NAME)).await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("one or more flags"), "{err}");
}

#[tokio::test]
async fn given_unknown_sandbox_when_updated_then_it_is_not_found() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            pids_max: Some(PIDS),
            ..change("ghost")
        })
        .await
        .unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::SandboxNotFound(_)));
}

#[tokio::test]
async fn given_restart_policy_when_updated_then_info_reports_it() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let updated = mgr
        .update(UpdateSandboxRequest {
            restart_policy: Some(PbRestartPolicy {
                name: "on-failure".into(),
                maximum_retry_count: 3,
            }),
            ..change(NAME)
        })
        .await
        .unwrap();

    // Assert
    let policy = updated.restart_policy.expect("policy");
    assert_eq!(
        (policy.name.as_str(), policy.maximum_retry_count),
        ("on-failure", 3)
    );
}

#[tokio::test]
async fn given_auto_remove_sandbox_when_restart_policy_set_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(
        CreateSandboxRequest {
            auto_remove: true,
            ..sized()
        },
        "",
    )
    .await
    .unwrap();

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            restart_policy: Some(PbRestartPolicy {
                name: "always".into(),
                maximum_retry_count: 0,
            }),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("auto_remove"), "{err}");
}

#[tokio::test]
async fn given_updated_limits_when_daemon_restarts_then_they_are_kept() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let first = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = first.create(sized(), "").await.unwrap().id;
    first
        .update(UpdateSandboxRequest {
            pids_max: Some(PIDS),
            nano_cpus: Some(HALF_CPU),
            ..change(&id)
        })
        .await
        .unwrap();

    // Act
    let second = restart(tmp.path()).await;

    // Assert
    let resources = limits_of(&second, &id).await;
    assert_eq!((resources.pids_max, resources.nano_cpus), (PIDS, HALF_CPU));
}

#[tokio::test]
async fn given_update_when_events_read_then_update_is_announced() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();
    let mut reader = mgr.events().subscribe(Some(std::time::UNIX_EPOCH));

    // Act
    mgr.update(UpdateSandboxRequest {
        pids_max: Some(PIDS),
        ..change(NAME)
    })
    .await
    .unwrap();

    // Assert
    let mut last = None;
    while let Ok(Some(next)) = tokio::time::timeout(EVENT_WAIT, reader.next()).await {
        last = Some(next.event.action);
    }
    assert_eq!(last, Some(EventAction::Update));
}
