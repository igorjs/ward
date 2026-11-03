// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `pause` and `unpause`: freezing a running sandbox's processes and thawing
//! them again, with Docker's refusals and events.

use super::{ApiError, CommandView, EventAction, PbSandboxInfo, Result, SandboxManager};
use crate::protocol::SandboxStatus;

impl SandboxManager {
    /// Freeze every process of a running sandbox. Its VM and agent stay up,
    /// so `cp`, `top`, `stats` and `logs` still work.
    pub async fn pause(&self, reference: &str) -> Result<PbSandboxInfo> {
        let id = self.resolve(reference).await?;
        self.require_running(&id).await?;
        if self.is_paused(&id).await? {
            return Err(ApiError::InvalidRequest(format!(
                "Container {id} is already paused"
            )));
        }
        self.backend
            .pause_sandbox(&id)
            .await
            .map_err(super::backend_err)?;
        tracing::info!(sandbox_id = %id, "sandbox paused");
        self.emit_event(&id, EventAction::Pause, None).await;
        self.info(&id, CommandView::Full).await
    }

    /// Thaw a paused sandbox.
    pub async fn unpause(&self, reference: &str) -> Result<PbSandboxInfo> {
        let id = self.resolve(reference).await?;
        self.require_running(&id).await?;
        if !self.is_paused(&id).await? {
            return Err(ApiError::InvalidRequest(format!(
                "Container {id} is not paused"
            )));
        }
        self.thaw(&id).await?;
        self.info(&id, CommandView::Full).await
    }

    /// Refuse to start a process in a paused sandbox, as `docker exec` does.
    pub(super) async fn refuse_if_paused(&self, id: &str) -> Result<()> {
        if self.is_paused(id).await? {
            return Err(ApiError::InvalidRequest(format!(
                "Container {id} is paused, unpause the container before exec"
            )));
        }
        Ok(())
    }

    /// Thaw the sandbox if it is paused. Stop and kill call this so the
    /// signal they send is delivered.
    pub(super) async fn thaw_if_paused(&self, id: &str) -> Result<()> {
        if self.is_paused(id).await? {
            self.thaw(id).await?;
        }
        Ok(())
    }

    async fn thaw(&self, id: &str) -> Result<()> {
        self.backend
            .unpause_sandbox(id)
            .await
            .map_err(super::backend_err)?;
        tracing::info!(sandbox_id = %id, "sandbox unpaused");
        self.emit_event(id, EventAction::Unpause, None).await;
        Ok(())
    }

    async fn require_running(&self, id: &str) -> Result<()> {
        if self.recorded_status(id).await? != SandboxStatus::Running {
            return Err(super::not_running(id));
        }
        Ok(())
    }

    async fn is_paused(&self, id: &str) -> Result<bool> {
        if self.recorded_status(id).await? != SandboxStatus::Running {
            return Ok(false);
        }
        let info = self
            .backend
            .get_sandbox(id)
            .await
            .map_err(super::backend_err)?;
        Ok(info.paused)
    }
}
