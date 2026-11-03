// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `update`: changing the limits and restart policy of a sandbox that
//! exists, running or not.

use super::{
    ApiError, Arc, CommandView, EventAction, PbSandboxInfo, RESTART_WITHOUT_SECRETS, Result,
    SandboxEvent, SandboxManager, backend_err,
};
use crate::pb::UpdateSandboxRequest;
use crate::protocol::{LimitsUpdate, RestartKind, RestartPolicy};
use crate::sync::LockUnpoisoned;

/// Docker's wording for `docker update` without a flag.
const NOTHING_TO_UPDATE: &str = "You must provide one or more flags when using this command.";

impl SandboxManager {
    /// Apply `req` to the sandbox it names. A running sandbox gets the new
    /// limits at once, inside its VM; the record keeps them for the next
    /// boot, where memory and CPUs also size the VM. The record is written
    /// before success is reported, and a failed write puts the old limits
    /// back in the VM.
    pub async fn update(&self, req: UpdateSandboxRequest) -> Result<PbSandboxInfo> {
        let id = self.resolve(&req.id).await?;
        let limits = LimitsUpdate {
            memory_mb: req.memory_mb,
            nano_cpus: req.nano_cpus,
            pids_max: req.pids_max,
        };
        if limits.is_empty() && req.restart_policy.is_none() {
            return Err(ApiError::InvalidRequest(NOTHING_TO_UPDATE.into()));
        }
        let lock = self.update_lock(&id);
        let _held = lock.lock().await;
        let restart = self
            .checked_restart(&id, req.restart_policy.as_ref())
            .await?;
        let previous = self.recorded_limits(&id).await?;
        if !limits.is_empty() {
            let next = previous.updated(&limits);
            crate::validate::resource_limits(
                next.cpus,
                next.memory_mb,
                next.pids_max,
                next.timeout_seconds,
            )?;
            self.backend
                .update_limits(&id, &limits)
                .await
                .map_err(backend_err)?;
        }
        if let Err(e) = self.record_update(&id, &limits, restart).await {
            return Err(match self.restore_limits(&id, &limits, &previous).await {
                Ok(()) => e,
                Err(stuck) => ApiError::Internal(format!(
                    "{e}; the old limits could not be put back, so the running sandbox may differ from its record: {stuck}"
                )),
            });
        }
        tracing::info!(sandbox = %id, "sandbox updated");
        self.info(&id, CommandView::Full).await
    }

    /// Put back the values `limits` changed, after the record could not be
    /// written. When that fails too, the live VM differs from the record.
    async fn restore_limits(
        &self,
        id: &str,
        limits: &LimitsUpdate,
        previous: &crate::protocol::ResourceLimits,
    ) -> std::result::Result<(), crate::backend::BackendError> {
        let undo = LimitsUpdate {
            memory_mb: limits.memory_mb.map(|_| previous.memory_mb),
            nano_cpus: limits.nano_cpus.map(|_| previous.nano_cpus),
            pids_max: limits.pids_max.map(|_| previous.pids_max),
        };
        if undo.is_empty() {
            return Ok(());
        }
        let result = self.backend.update_limits(id, &undo).await;
        if let Err(e) = &result {
            tracing::warn!(sandbox_id = %id, error = %e, "cannot put the old limits back");
        }
        result
    }

    fn update_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.update_locks.lock_unpoisoned();
        Arc::clone(locks.entry(id.to_string()).or_default())
    }

    /// Re-apply a CPU quota to a freshly booted VM, which starts unlimited.
    /// A quota that cannot be applied fails the start, because the sandbox
    /// would run without the limit `inspect` reports.
    pub(super) async fn reapply_cpu_quota(&self, id: &str) -> Result<()> {
        let lock = self.update_lock(id);
        let _held = lock.lock().await;
        let limits = self.recorded_limits(id).await?;
        if limits.nano_cpus == 0 {
            return Ok(());
        }
        let update = LimitsUpdate {
            nano_cpus: Some(limits.nano_cpus),
            ..LimitsUpdate::default()
        };
        self.backend.update_limits(id, &update).await.map_err(|e| {
            ApiError::Internal(format!(
                "cannot apply the CPU limit of {} nano CPUs: {e}",
                limits.nano_cpus
            ))
        })
    }

    async fn checked_restart(
        &self,
        id: &str,
        policy: Option<&crate::pb::RestartPolicy>,
    ) -> Result<Option<RestartPolicy>> {
        let Some(policy) = policy else {
            return Ok(None);
        };
        let entries = self.entries.read().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        let restart =
            crate::validate::restart_policy(Some(policy), entry.meta.options.spec.auto_remove)?;
        // A fresh VM has no secrets, so a restart could not deliver them.
        if entry.meta.had_secrets && restart.kind != RestartKind::No {
            return Err(ApiError::InvalidRequest(RESTART_WITHOUT_SECRETS.into()));
        }
        Ok(Some(restart))
    }

    async fn recorded_limits(&self, id: &str) -> Result<crate::protocol::ResourceLimits> {
        self.entries
            .read()
            .await
            .get(id)
            .map(|entry| entry.meta.options.resources.clone())
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))
    }

    async fn record_update(
        &self,
        id: &str,
        limits: &LimitsUpdate,
        restart: Option<RestartPolicy>,
    ) -> Result<()> {
        let mut entries = self.entries.write().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
        let mut next = entry.meta.clone();
        next.options.resources = next.options.resources.updated(limits);
        if let Some(restart) = restart {
            next.options.spec.restart = restart;
        }
        self.persist(next.clone()).await?;
        entry.meta = next;
        self.events
            .emit(SandboxEvent::new(EventAction::Update, &entry.meta));
        Ok(())
    }
}
