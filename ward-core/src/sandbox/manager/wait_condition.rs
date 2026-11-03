// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `wait` with Docker's conditions: `not-running`, `next-exit` and `removed`.

use super::{ApiError, Result, SandboxManager, WAIT_POLL, decode_wait_status, watch};
use crate::pb::WaitCondition;
use crate::protocol::SandboxStatus;

impl SandboxManager {
    /// Block until `condition` holds and return the main process's exit code.
    pub async fn wait_until(&self, reference: &str, condition: WaitCondition) -> Result<i32> {
        let id = self.resolve(reference).await?;
        match condition {
            WaitCondition::NotRunning => self.wait(&id).await,
            WaitCondition::NextExit => {
                self.await_running(&id).await?;
                self.wait(&id).await
            }
            WaitCondition::Removed => self.wait_removed(&id).await,
        }
    }

    /// Return once the sandbox runs, so the exit that follows is the next one.
    async fn await_running(&self, id: &str) -> Result<()> {
        loop {
            let status = self
                .entries
                .read()
                .await
                .get(id)
                .map(|entry| entry.meta.status.clone())
                .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
            if status == SandboxStatus::Running {
                return Ok(());
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Return the last exit code once the sandbox is gone, or 0 when it never
    /// ran a command.
    async fn wait_removed(&self, id: &str) -> Result<i32> {
        let mut live: Option<watch::Receiver<Option<i32>>> = None;
        let mut last = None;
        loop {
            let recorded = self
                .entries
                .read()
                .await
                .get(id)
                .map(|entry| (entry.meta.status.clone(), entry.meta.exit_code));
            let Some((status, exit_code)) = recorded else {
                // A removal right after the exit can beat the next poll, so
                // the live status is read before the record.
                let ended = live.and_then(|exit| exit.borrow().map(decode_wait_status));
                return Ok(ended.or(last).unwrap_or_default());
            };
            last = exit_code.or(last);
            if status == SandboxStatus::Running && live.is_none() {
                live = self.running_main_exit(id).await;
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }
}
