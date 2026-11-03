// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Runs the health checks of running sandboxes by exec in the guest.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::{EventAction, SandboxManager, health};
use crate::pb::ExecRequest;
use crate::protocol::{SandboxStatus, StreamEventKind};

/// How often the supervisor looks for checks that are due.
const TICK: Duration = Duration::from_millis(500);

/// How long to wait for a check that ran out of time to be killed.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// A check that is due, with what it needs to run.
pub(super) struct Due {
    pub(super) id: String,
    pub(super) check: health::Check,
    /// The run it was claimed for, so a late result cannot reach a newer run.
    pub(super) run_started: Option<SystemTime>,
}

impl SandboxManager {
    /// Run health checks as they fall due, for as long as the manager lives.
    pub fn spawn_health_supervisor(self: &Arc<Self>) {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            loop {
                ticker.tick().await;
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                for due in manager.take_due_checks().await {
                    let manager = Arc::clone(&manager);
                    tokio::spawn(async move { manager.run_check(due).await });
                }
            }
        });
    }

    /// Start tracking new runs, and claim the checks that are due now.
    pub(super) async fn take_due_checks(&self) -> Vec<Due> {
        let now = Instant::now();
        let mut due = Vec::new();
        let mut entries = self.entries.write().await;
        for entry in entries.values_mut() {
            let config = entry.meta.options.spec.guest.healthcheck.as_ref();
            let Some(check) = config.and_then(health::Check::from_config) else {
                continue;
            };
            if entry.meta.status != SandboxStatus::Running {
                continue;
            }
            let run = entry.meta.started_at;
            let stale = entry.health.as_ref().is_none_or(|t| t.run_started != run);
            if stale {
                // Dates the run from its real start, which the supervisor may see late.
                let age = run
                    .and_then(|r| SystemTime::now().duration_since(r).ok())
                    .unwrap_or_default();
                let started = now.checked_sub(age).unwrap_or(now);
                entry.health = Some(health::Tracker::new(run, started, check.interval));
            }
            if entry.health.as_mut().is_some_and(|t| t.take_due(now)) {
                due.push(Due {
                    id: entry.meta.id.clone(),
                    check,
                    run_started: run,
                });
            }
        }
        due
    }

    /// Run one check, record it, and report a change of status.
    pub(super) async fn run_check(&self, due: Due) {
        // A frozen guest cannot run a check, and Docker skips it too.
        if self.is_paused(&due.id).await.unwrap_or(false) {
            let mut entries = self.entries.write().await;
            let tracker = entries.get_mut(&due.id).and_then(|e| e.health.as_mut());
            if let Some(tracker) = tracker.filter(|t| t.run_started == due.run_started) {
                tracker.skip(&due.check, Instant::now());
            }
            return;
        }
        let began = Instant::now();
        let probe = self.probe(&due.id, &due.check).await;
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(&due.id) else {
            return;
        };
        let Some(tracker) = entry.health.as_mut() else {
            return;
        };
        if tracker.run_started != due.run_started {
            return;
        }
        let Some(status) = tracker.record(probe, &due.check, began, Instant::now()) else {
            return;
        };
        let action = match status {
            health::STATUS_HEALTHY => EventAction::HealthHealthy,
            health::STATUS_UNHEALTHY => EventAction::HealthUnhealthy,
            _ => return,
        };
        self.events
            .emit(super::event_with_code(action, &entry.meta, None));
    }

    /// Exec the check in the guest and wait for it, up to its timeout. A
    /// process that cannot start counts as a failure with the reason as its
    /// output, and one that runs out of time is killed.
    async fn probe(&self, id: &str, check: &health::Check) -> health::Probe {
        let start = SystemTime::now();
        let (exit_code, output) = self.run_in_guest(id, check).await;
        health::Probe {
            start,
            end: SystemTime::now(),
            exit_code,
            output: health::truncate_output(&output),
        }
    }

    async fn run_in_guest(&self, id: &str, check: &health::Check) -> (i32, String) {
        let deadline = tokio::time::Instant::now() + check.timeout;
        let timed_out = || {
            (
                health::FAILED_EXIT_CODE,
                format!("Health check exceeded timeout ({:?})", check.timeout),
            )
        };
        let request = ExecRequest {
            sandbox_id: id.to_string(),
            command: check.argv.clone(),
            ..Default::default()
        };
        let process = match tokio::time::timeout_at(deadline, self.exec(request)).await {
            Ok(Ok(process)) => process,
            Ok(Err(e)) => return (health::FAILED_EXIT_CODE, e.to_string()),
            Err(_) => return timed_out(),
        };
        let streamed = tokio::time::timeout_at(deadline, async {
            let mut events = self.stream_output(id, &process.pid).await?;
            Ok::<_, crate::protocol::ApiError>(collect_output(&mut events).await)
        })
        .await;
        let result = match streamed {
            Ok(Ok(result)) => return result,
            Ok(Err(e)) => (health::FAILED_EXIT_CODE, e.to_string()),
            Err(_) => timed_out(),
        };
        let _ = tokio::time::timeout(KILL_TIMEOUT, self.kill_process(id, &process.pid)).await;
        result
    }
}

/// Read a check's events to its exit. Output past Docker's limit is read
/// and dropped, so a noisy check cannot grow memory.
async fn collect_output(
    events: &mut tokio::sync::mpsc::Receiver<crate::protocol::StreamEvent>,
) -> (i32, String) {
    let mut output = String::new();
    while let Some(event) = events.recv().await {
        match event.kind {
            StreamEventKind::Exit => {
                let code = event.exit_code.unwrap_or(health::FAILED_EXIT_CODE);
                return (code, output);
            }
            StreamEventKind::Data { bytes, .. } => {
                health::append_capped(&mut output, &String::from_utf8_lossy(&bytes));
            }
            _ => {
                health::append_capped(&mut output, &event.line);
                health::append_capped(&mut output, "\n");
            }
        }
    }
    (health::FAILED_EXIT_CODE, output)
}
