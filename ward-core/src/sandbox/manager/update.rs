// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `update`: changing the limits and restart policy of a sandbox that
//! exists, running or not.

use super::{
    ApiError, CommandView, EventAction, PbSandboxInfo, RESTART_WITHOUT_SECRETS, Result,
    SandboxEvent, SandboxManager, backend_err,
};
use crate::pb::UpdateSandboxRequest;
use crate::protocol::{LimitsUpdate, RestartKind, RestartPolicy};

/// Docker's wording for `docker update` without a flag.
const NOTHING_TO_UPDATE: &str = "You must provide one or more flags when using this command.";

impl SandboxManager {
    /// Apply `req` to the sandbox it names. A running sandbox gets the new
    /// limits at once, inside its VM; the record keeps them for the next
    /// boot, where memory and CPUs also size the VM.
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
        let restart = self
            .checked_restart(&id, req.restart_policy.as_ref())
            .await?;
        if !limits.is_empty() {
            let current = self.recorded_limits(&id).await?;
            let next = current.updated(&limits);
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
        self.record_update(&id, &limits, restart).await?;
        tracing::info!(sandbox = %id, "sandbox updated");
        self.info(&id, CommandView::Full).await
    }

    /// Re-apply a CPU quota to a freshly booted VM, which starts unlimited.
    pub(super) async fn reapply_cpu_quota(&self, id: &str) {
        let Ok(limits) = self.recorded_limits(id).await else {
            return;
        };
        if limits.nano_cpus == 0 {
            return;
        }
        let update = LimitsUpdate {
            nano_cpus: Some(limits.nano_cpus),
            ..LimitsUpdate::default()
        };
        if let Err(e) = self.backend.update_limits(id, &update).await {
            tracing::warn!(sandbox_id = %id, error = %e, "cannot re-apply the CPU limit");
        }
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
