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

#[tokio::test]
async fn given_a_record_that_cannot_be_written_when_limits_updated_then_the_old_limits_are_put_back()
 {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;
    // A directory where the record's temp file goes makes every save fail.
    let dir = mgr.store.as_ref().expect("store").sandbox_dir(&id);
    std::fs::create_dir(dir.join("meta.json.tmp")).unwrap();

    // Act
    let result = mgr
        .update(UpdateSandboxRequest {
            memory_mb: Some(SMALLER_MEMORY_MB),
            pids_max: Some(PIDS),
            ..change(&id)
        })
        .await;

    // Assert
    assert!(result.is_err());
    let live = mgr.backend.get_sandbox(&id).await.unwrap().resources;
    assert_eq!((live.memory_mb, live.pids_max), (VM_MEMORY_MB, 0));
    assert_eq!(limits_of(&mgr, &id).await.memory_mb, VM_MEMORY_MB);
}

#[tokio::test]
async fn given_a_stored_cpu_quota_the_vm_cannot_take_when_started_then_the_start_fails() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;
    mgr.stop_within(&id, Some(0)).await.unwrap();
    mgr.entries
        .write()
        .await
        .get_mut(&id)
        .expect("entry")
        .meta
        .options
        .resources
        .nano_cpus = THREE_CPUS;

    // Act
    let err = mgr.start(&id).await.unwrap_err();

    // Assert
    assert!(
        matches!(&err, ApiError::Internal(m) if m.contains("cannot apply the CPU limit")),
        "{err}"
    );
    assert_eq!(
        mgr.get(&id).await.unwrap().status,
        crate::pb::SandboxStatus::Failed as i32
    );
}

#[tokio::test]
async fn given_concurrent_updates_when_they_finish_then_record_and_vm_agree() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;
    let spawn_update = |memory_mb: u32, pids_max: u32| {
        let (mgr, id) = (Arc::clone(&mgr), id.clone());
        tokio::spawn(async move {
            mgr.update(UpdateSandboxRequest {
                memory_mb: Some(memory_mb),
                pids_max: Some(pids_max),
                ..change(&id)
            })
            .await
        })
    };
    let first = spawn_update(SMALLER_MEMORY_MB, PIDS);
    let second = spawn_update(SMALLER_MEMORY_MB / 2, PIDS * 2);

    // Act
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    // Assert
    let recorded = limits_of(&mgr, &id).await;
    let live = mgr.backend.get_sandbox(&id).await.unwrap().resources;
    assert_eq!(
        (recorded.memory_mb, recorded.pids_max),
        (live.memory_mb, live.pids_max)
    );
}

#[tokio::test]
async fn given_updated_sandbox_when_removed_then_its_update_lock_is_dropped() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    let id = mgr.create(sized(), "").await.unwrap().id;
    mgr.update(UpdateSandboxRequest {
        pids_max: Some(PIDS),
        ..change(&id)
    })
    .await
    .unwrap();
    assert!(mgr.update_locks.lock_unpoisoned().contains_key(&id));

    // Act
    mgr.remove(&id).await.unwrap();

    // Assert
    assert!(!mgr.update_locks.lock_unpoisoned().contains_key(&id));
}

#[tokio::test]
async fn given_cgroup_options_when_updated_then_info_reports_them_and_the_rest_stay() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();
    mgr.update(UpdateSandboxRequest {
        cpu_shares: Some(512),
        cpuset_cpus: Some("0".into()),
        ..change(NAME)
    })
    .await
    .unwrap();

    // Act
    let updated = mgr
        .update(UpdateSandboxRequest {
            cpu_quota: Some(50_000),
            cpu_period: Some(100_000),
            memory_reservation_bytes: Some(64 << 20),
            ..change(NAME)
        })
        .await
        .unwrap();

    // Assert
    let cgroup = updated.guest.expect("guest").cgroup.expect("cgroup");
    assert_eq!(cgroup.cpu_shares, 512);
    assert_eq!(cgroup.cpuset_cpus, "0");
    assert_eq!(cgroup.cpu_quota, 50_000);
    assert_eq!(cgroup.cpu_period, 100_000);
    assert_eq!(cgroup.memory_reservation_bytes, 64 << 20);
}

#[tokio::test]
async fn given_a_swap_other_than_the_memory_limit_when_updated_then_it_is_refused_with_the_reason()
{
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            memory_swap_bytes: Some(-1),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("the guest has no swap"), "{err}");
}

#[tokio::test]
async fn given_a_reservation_above_the_memory_limit_when_updated_then_it_is_refused() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            memory_reservation_bytes: Some(u64::from(LARGER_MEMORY_MB) << 20),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("--memory-reservation"), "{err}");
}

#[tokio::test]
async fn given_a_bad_cpuset_when_updated_then_nothing_changes() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();

    // Act
    let err = mgr
        .update(UpdateSandboxRequest {
            cpuset_cpus: Some("3-1".into()),
            ..change(NAME)
        })
        .await
        .unwrap_err();

    // Assert
    assert!(err.to_string().contains("--cpuset-cpus"), "{err}");
    let info = mgr.get(NAME).await.unwrap();
    assert_eq!(
        info.guest
            .expect("guest")
            .cgroup
            .expect("cgroup")
            .cpuset_cpus,
        ""
    );
}

#[tokio::test]
async fn given_a_swap_equal_to_memory_when_the_memory_limit_changes_then_the_swap_follows() {
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager_on(tmp.path(), MAX_MANAGER_SANDBOXES);
    mgr.create(sized(), "").await.unwrap();
    let equal = i64::from(VM_MEMORY_MB) << 20;
    mgr.update(UpdateSandboxRequest {
        memory_swap_bytes: Some(equal),
        ..change(NAME)
    })
    .await
    .unwrap();

    // Act
    let updated = mgr
        .update(UpdateSandboxRequest {
            memory_mb: Some(SMALLER_MEMORY_MB),
            ..change(NAME)
        })
        .await
        .unwrap();

    // Assert
    let cgroup = updated.guest.expect("guest").cgroup.expect("cgroup");
    assert_eq!(cgroup.memory_swap_bytes, i64::from(SMALLER_MEMORY_MB) << 20);
}
