// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Record changes that write to disk without holding the map-wide lock.
//!
//! Lock order: a sandbox's update lock, then its persist lock, then the
//! `entries` lock. No code waits for a persist lock while holding `entries`,
//! and nothing holds a persist lock across a backend call except removal.

use super::{
    ApiError, EndReason, Mutex, Result, SandboxEntry, SandboxMeta, Teardown, persist_meta,
    rearm_exited_ttl, stamp_times,
};
use crate::protocol::SandboxStatus;
use crate::sync::LockUnpoisoned;
use std::sync::Arc;

/// How a status change treats the recorded end reason.
pub(super) enum ReasonUpdate {
    /// A new run clears it, every other move keeps it.
    Auto,
    Set(Option<EndReason>),
}

/// How a status change treats the recorded exit code.
pub(super) enum ExitUpdate {
    Keep,
    Set(Option<i32>),
}

/// The record after moving to `status`.
pub(super) fn status_meta(
    meta: &SandboxMeta,
    status: SandboxStatus,
    exit_code: ExitUpdate,
    reason: ReasonUpdate,
) -> SandboxMeta {
    let mut next = meta.clone();
    stamp_times(&mut next, &status, std::time::SystemTime::now());
    next.end_reason = match reason {
        ReasonUpdate::Set(reason) => reason,
        ReasonUpdate::Auto if status == SandboxStatus::Creating => None,
        ReasonUpdate::Auto => meta.end_reason,
    };
    next.status = status;
    if let ExitUpdate::Set(code) = exit_code {
        next.exit_code = code;
    }
    next
}

/// Held while one sandbox's record changes, so two writers for it never
/// interleave and the file always matches memory. Clones share the lock.
#[derive(Clone)]
pub(super) struct PersistGuard {
    teardown: Teardown,
    id: String,
    _held: Arc<tokio::sync::OwnedMutexGuard<()>>,
}

impl Teardown {
    pub(super) fn persist_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.persist_locks.lock_unpoisoned();
        Arc::clone(locks.entry(id.to_string()).or_default())
    }

    /// Drop the lock entry of a sandbox that is gone.
    pub(super) fn forget_persist_lock(&self, id: &str) {
        self.persist_locks.lock_unpoisoned().remove(id);
    }

    // A caller dropped while it waits here leaves the map entry until the
    // sandbox is removed, which is harmless.
    pub(super) async fn lock_persist(&self, id: &str) -> PersistGuard {
        let held = self.persist_lock(id).lock_owned().await;
        PersistGuard {
            teardown: self.clone(),
            id: id.to_string(),
            _held: Arc::new(held),
        }
    }

    /// Change one sandbox's record, see [`PersistGuard::commit`].
    pub(super) async fn commit_with<R: Send + 'static>(
        &self,
        id: &str,
        plan: impl FnOnce(&SandboxEntry) -> Result<Option<SandboxMeta>>,
        adopt: impl FnOnce(&mut SandboxEntry) -> R + Send + 'static,
    ) -> Result<Option<R>> {
        self.lock_persist(id).await.commit(plan, adopt).await
    }
}

impl PersistGuard {
    /// Build the next record from the current one with `plan`, write it
    /// with no map lock held, then adopt it and run `adopt` under the write
    /// lock. A plan that returns `None` changes nothing. A failed write
    /// leaves memory and disk as they were. `plan` runs under a read lock
    /// and the guard keeps every other writer of this sandbox out, so what
    /// it saw is still true when the record is adopted. The write and the
    /// adoption run in their own task, so a caller that is dropped midway
    /// cannot leave the file ahead of memory or free the lock early.
    pub(super) async fn commit<R: Send + 'static>(
        &self,
        plan: impl FnOnce(&SandboxEntry) -> Result<Option<SandboxMeta>>,
        adopt: impl FnOnce(&mut SandboxEntry) -> R + Send + 'static,
    ) -> Result<Option<R>> {
        self.commit_saving(/* save = */ true, plan, adopt).await
    }

    /// Like [`commit`](Self::commit) but leaves the file alone, so memory
    /// runs ahead of disk. For a fact that is already true outside the
    /// daemon, such as a dead VM, when the disk keeps refusing the write.
    pub(super) async fn commit_unsaved<R: Send + 'static>(
        &self,
        plan: impl FnOnce(&SandboxEntry) -> Result<Option<SandboxMeta>>,
        adopt: impl FnOnce(&mut SandboxEntry) -> R + Send + 'static,
    ) -> Result<Option<R>> {
        self.commit_saving(/* save = */ false, plan, adopt).await
    }

    async fn commit_saving<R: Send + 'static>(
        &self,
        save: bool,
        plan: impl FnOnce(&SandboxEntry) -> Result<Option<SandboxMeta>>,
        adopt: impl FnOnce(&mut SandboxEntry) -> R + Send + 'static,
    ) -> Result<Option<R>> {
        let next = {
            let entries = self.teardown.entries.read().await;
            let Some(entry) = entries.get(&self.id) else {
                return Err(self.gone());
            };
            match plan(entry)? {
                Some(next) => next,
                None => return Ok(None),
            }
        };
        let guard = self.clone();
        let write = tokio::spawn(async move {
            if save {
                persist_meta(guard.teardown.store.as_ref(), next.clone()).await?;
            }
            let mut entries = guard.teardown.entries.write().await;
            let Some(entry) = entries.get_mut(&guard.id) else {
                // Removal waits for this guard before it deletes the record.
                return Err(guard.gone());
            };
            entry.meta = next;
            let adopted = adopt(entry);
            guard.teardown.run_changes.send_replace(());
            Ok(Some(adopted))
        });
        write
            .await
            .map_err(|e| ApiError::Internal(format!("record write task: {e}")))?
    }

    /// Move to `status` if `check` agrees, and re-arm the exited TTL.
    pub(super) async fn commit_status<R: Send + 'static>(
        &self,
        status: SandboxStatus,
        exit_code: ExitUpdate,
        reason: ReasonUpdate,
        check: impl FnOnce(&SandboxEntry) -> Result<bool>,
        extra: impl FnOnce(&mut SandboxEntry) -> R + Send + 'static,
    ) -> Result<Option<R>> {
        let teardown = self.teardown.clone();
        self.commit(
            |entry| Ok(check(entry)?.then(|| status_meta(&entry.meta, status, exit_code, reason))),
            move |entry| {
                rearm_exited_ttl(&teardown, entry, std::time::SystemTime::now());
                extra(entry)
            },
        )
        .await
    }

    fn gone(&self) -> ApiError {
        self.teardown.forget_persist_lock(&self.id);
        ApiError::SandboxNotFound(self.id.clone())
    }
}
