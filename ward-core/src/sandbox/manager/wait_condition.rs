// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `wait` with Docker's conditions: `not-running`, `next-exit` and `removed`.

use super::{ApiError, Result, SandboxManager, WAIT_POLL, decode_wait_status, watch};
use crate::pb::WaitCondition;
use crate::protocol::SandboxStatus;

impl SandboxManager {
    /// Block until `condition` holds and return the main process's exit code.
    pub async fn wait_until(&self, reference: &str, condition: WaitCondition) -> Result<i32> {
        let id = self.resolve(reference).await?;
        let baseline = self.runs_started(&id).await?;
        match condition {
            WaitCondition::NotRunning => self.wait(&id).await,
            WaitCondition::NextExit => {
                self.await_run_after(&id, baseline).await?;
                self.wait(&id).await
            }
            WaitCondition::Removed => self.wait_removed(&id).await,
        }
    }

    /// How many runs the sandbox has started so far.
    pub(super) async fn runs_started(&self, id: &str) -> Result<u64> {
        self.entries
            .read()
            .await
            .get(id)
            .map(|entry| entry.runs_started)
            .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))
    }

    /// Return once the sandbox is running, or has run since `baseline`, so
    /// the exit that follows is the next one. A run that already ended
    /// still counts, because its exit is the one the caller asked for.
    pub(super) async fn await_run_after(&self, id: &str, baseline: u64) -> Result<()> {
        let mut changes = self.run_changes.subscribe();
        loop {
            let (status, runs) = self
                .entries
                .read()
                .await
                .get(id)
                .map(|entry| (entry.meta.status.clone(), entry.runs_started))
                .ok_or_else(|| ApiError::SandboxNotFound(id.to_string()))?;
            if status == SandboxStatus::Running || runs > baseline {
                return Ok(());
            }
            // The sender lives as long as the manager, so this cannot fail.
            let _ = changes.changed().await;
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
                let from_vm = self.recorded_vm_exit_code(id);
                return Ok(from_vm.or(ended).or(last).unwrap_or_default());
            };
            last = exit_code.or(last);
            if status == SandboxStatus::Running && live.is_none() {
                live = self.running_main_exit(id).await;
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }
}
