// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Runs the health checks of running sandboxes by exec in the guest.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::{EventAction, SandboxManager, health};
use crate::pb::ExecRequest;
use crate::protocol::{SandboxStatus, StreamEventKind};

/// How often the supervisor looks for checks that are due.
const TICK: Duration = Duration::from_millis(500);

/// A check that is due, with what it needs to run.
struct Due {
    id: String,
    check: health::Check,
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
    async fn take_due_checks(&self) -> Vec<Due> {
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
                entry.health = Some(health::Tracker::new(run, now, check.interval));
            }
            if entry.health.as_mut().is_some_and(|t| t.take_due(now)) {
                due.push(Due {
                    id: entry.meta.id.clone(),
                    check,
                });
            }
        }
        due
    }

    /// Run one check, record it, and report a change of status.
    async fn run_check(&self, due: Due) {
        let probe = self.probe(&due.id, &due.check).await;
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(&due.id) else {
            return;
        };
        let Some(tracker) = entry.health.as_mut() else {
            return;
        };
        let Some(status) = tracker.record(probe, &due.check, Instant::now()) else {
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
        let request = ExecRequest {
            sandbox_id: id.to_string(),
            command: check.argv.clone(),
            ..Default::default()
        };
        let process = match self.exec(request).await {
            Ok(process) => process,
            Err(e) => return (health::FAILED_EXIT_CODE, e.to_string()),
        };
        let mut events = match self.stream_output(id, &process.pid).await {
            Ok(events) => events,
            Err(e) => return (health::FAILED_EXIT_CODE, e.to_string()),
        };
        let collect = async {
            let mut output = String::new();
            while let Some(event) = events.recv().await {
                match event.kind {
                    StreamEventKind::Exit => {
                        let code = event.exit_code.unwrap_or(health::FAILED_EXIT_CODE);
                        return (code, output);
                    }
                    StreamEventKind::Data { bytes, .. } => {
                        output.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    _ => {
                        output.push_str(&event.line);
                        output.push('\n');
                    }
                }
            }
            (health::FAILED_EXIT_CODE, output)
        };
        if let Ok(result) = tokio::time::timeout(check.timeout, collect).await {
            return result;
        }
        let _ = self.kill_process(id, &process.pid).await;
        (
            health::FAILED_EXIT_CODE,
            format!("Health check exceeded timeout ({:?})", check.timeout),
        )
    }
}
