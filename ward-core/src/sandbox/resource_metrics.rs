// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Per-sandbox resource gauges for the Prometheus endpoint.
//!
//! A collector task reads the stats of every running sandbox on a timer and
//! sets one labelled series per sandbox and metric. A sandbox that stops or
//! is removed stops being updated, and the exporter drops its series once
//! they have been idle for [`SERIES_IDLE_TIMEOUT`], so cardinality follows
//! the running sandboxes.
//!
//! Cumulative values are gauges because the `metrics` counter type cannot
//! go back to zero when a sandbox restarts. They only grow within one run,
//! so `rate()` and `increase()` work on them.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::{SandboxManager, SandboxStats};
use crate::pb::SandboxStatus;

/// Time between two readings of the running sandboxes.
pub const COLLECT_INTERVAL: Duration = Duration::from_secs(15);
/// How long a series may go without an update before the exporter drops it:
/// a few missed collections.
pub const SERIES_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

const SANDBOX_LABEL: &str = "sandbox_id";
const NANOS_PER_SECOND: f64 = 1e9;

pub const CPU_SECONDS: &str = "ward_sandbox_cpu_seconds_total";
pub const MEMORY_BYTES: &str = "ward_sandbox_memory_bytes";
pub const MEMORY_PEAK_BYTES: &str = "ward_sandbox_memory_peak_bytes";
pub const BLOCK_READ_BYTES: &str = "ward_sandbox_block_read_bytes_total";
pub const BLOCK_WRITE_BYTES: &str = "ward_sandbox_block_write_bytes_total";
pub const NET_RX_BYTES: &str = "ward_sandbox_net_rx_bytes_total";
pub const NET_TX_BYTES: &str = "ward_sandbox_net_tx_bytes_total";

/// Set the series of sandbox `id` from one reading.
pub fn record(id: &str, stats: &SandboxStats) {
    let gauge = |name: &'static str, value: f64| {
        metrics::gauge!(name, SANDBOX_LABEL => id.to_string()).set(value);
    };
    gauge(CPU_SECONDS, stats.cpu_nanos as f64 / NANOS_PER_SECOND);
    gauge(MEMORY_BYTES, stats.mem_used_bytes as f64);
    gauge(MEMORY_PEAK_BYTES, stats.mem_peak_bytes as f64);
    gauge(BLOCK_READ_BYTES, stats.block_read_bytes as f64);
    gauge(BLOCK_WRITE_BYTES, stats.block_write_bytes as f64);
    gauge(NET_RX_BYTES, stats.net_rx_bytes as f64);
    gauge(NET_TX_BYTES, stats.net_tx_bytes as f64);
}

/// The stats of every running sandbox. One that cannot be read right now
/// (it stopped meanwhile, or its agent is busy) is left out.
async fn running_stats(manager: &SandboxManager) -> Vec<(String, SandboxStats)> {
    let listed = match manager.list().await {
        Ok(listed) => listed,
        Err(e) => {
            tracing::warn!(error = %e, "listing sandboxes for resource metrics failed");
            return Vec::new();
        }
    };
    let mut readings = Vec::new();
    for info in listed
        .into_iter()
        .filter(|info| info.status == SandboxStatus::Running as i32)
    {
        match manager.stats(&info.id).await {
            Ok(stats) => readings.push((info.id, stats)),
            Err(e) => {
                tracing::debug!(sandbox_id = %info.id, error = %e, "no resource metrics this round");
            }
        }
    }
    readings
}

/// Publish the resource gauges of the running sandboxes every
/// [`COLLECT_INTERVAL`] until the task is aborted.
pub fn spawn_collector(manager: Arc<SandboxManager>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(COLLECT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for (id, stats) in running_stats(&manager).await {
                record(&id, &stats);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    const GIB: u64 = 1 << 30;

    fn stats() -> SandboxStats {
        SandboxStats {
            cpu_nanos: 2_500_000_000,
            online_cpus: 2,
            mem_used_bytes: GIB,
            mem_limit_bytes: 2 * GIB,
            pids: 3,
            read_at: std::time::SystemTime::UNIX_EPOCH,
            mem_peak_bytes: GIB + 1,
            block_read_bytes: 10,
            block_write_bytes: 20,
            net_rx_bytes: 30,
            net_tx_bytes: 40,
        }
    }

    #[test]
    fn given_a_reading_when_recorded_then_each_metric_is_labelled_with_the_sandbox() {
        // Arrange
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        // Act
        metrics::with_local_recorder(&recorder, || record("sb-1", &stats()));

        // Assert
        let mut seen: Vec<(String, String, f64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, value)| {
                let DebugValue::Gauge(value) = value else {
                    panic!("{} is not a gauge", key.key().name());
                };
                let label = key.key().labels().next().expect("a label");
                (
                    key.key().name().to_string(),
                    format!("{}={}", label.key(), label.value()),
                    value.into_inner(),
                )
            })
            .collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        let expected = [
            (BLOCK_READ_BYTES, 10.0),
            (BLOCK_WRITE_BYTES, 20.0),
            (CPU_SECONDS, 2.5),
            (MEMORY_BYTES, GIB as f64),
            (MEMORY_PEAK_BYTES, (GIB + 1) as f64),
            (NET_RX_BYTES, 30.0),
            (NET_TX_BYTES, 40.0),
        ];
        assert_eq!(seen.len(), expected.len());
        for ((name, label, value), (want_name, want_value)) in seen.iter().zip(expected) {
            assert_eq!(
                (name.as_str(), label.as_str()),
                (want_name, "sandbox_id=sb-1")
            );
            assert!((value - want_value).abs() < f64::EPSILON, "{name}: {value}");
        }
    }

    #[cfg(not(feature = "krunvm"))]
    #[tokio::test]
    async fn given_running_and_stopped_sandboxes_when_collected_then_only_running_ones_are_read() {
        // Arrange
        let mgr = crate::sandbox::host_manager(1024);
        let running =
            crate::sandbox::create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        let stopped =
            crate::sandbox::create_with_command(&mgr, &["sh", "-c", "read x"], false).await;
        mgr.stop(&stopped).await.expect("stop");

        // Act
        let readings = running_stats(&mgr).await;

        // Assert
        let ids: Vec<_> = readings.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, [running.as_str()]);
    }
}
