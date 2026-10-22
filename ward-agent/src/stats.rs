// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest-wide resource counters read from `/proc`.

use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use crate::proto::StatsSample;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const BYTES_PER_KIB: u64 = 1024;
/// Used when the kernel reports no clock tick rate.
const FALLBACK_TICKS_PER_SECOND: u64 = 100;

/// Indexes into the aggregate `cpu` line of `/proc/stat`: user, nice,
/// system, irq, softirq and steal. Idle, iowait and the guest columns (the
/// kernel counts those inside user and nice) are left out.
const CPU_BUSY_FIELDS: [usize; 6] = [0, 1, 2, 5, 6, 7];

fn invalid(what: &str) -> Error {
    Error::new(ErrorKind::InvalidData, what.to_string())
}

/// Read one sample from the `/proc` tree at `proc_root`.
pub fn sample(proc_root: &Path) -> Result<StatsSample> {
    let ticks = match rustix::param::clock_ticks_per_second() {
        0 => FALLBACK_TICKS_PER_SECOND,
        ticks => ticks,
    };
    let (cpu_nanos, online_cpus) =
        parse_cpu(&std::fs::read_to_string(proc_root.join("stat"))?, ticks)?;
    let (mem_total_bytes, mem_available_bytes) =
        parse_memory(&std::fs::read_to_string(proc_root.join("meminfo"))?)?;
    Ok(StatsSample {
        cpu_nanos,
        online_cpus,
        mem_total_bytes,
        mem_available_bytes,
        pids: count_processes(proc_root)?,
    })
}

/// Busy CPU time in nanoseconds and the number of CPUs from `/proc/stat`.
fn parse_cpu(text: &str, ticks_per_second: u64) -> Result<(u64, u32)> {
    let mut busy_ticks = None;
    let mut cpus: u32 = 0;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(label) = words.next() else { continue };
        if label == "cpu" {
            let fields: Vec<u64> = words.map_while(|w| w.parse().ok()).collect();
            // Older kernels omit the trailing columns, which count as zero.
            let busy = CPU_BUSY_FIELDS
                .iter()
                .map(|&i| fields.get(i).copied().unwrap_or(0))
                .fold(0u64, u64::saturating_add);
            busy_ticks = Some(busy);
        } else if label
            .strip_prefix("cpu")
            .is_some_and(|n| n.parse::<u32>().is_ok())
        {
            cpus += 1;
        }
    }
    let busy = busy_ticks.ok_or_else(|| invalid("no cpu line in stat"))?;
    let nanos = busy.saturating_mul(NANOS_PER_SECOND / ticks_per_second.max(1));
    Ok((nanos, cpus.max(1)))
}

/// Total and available memory in bytes from `/proc/meminfo`.
fn parse_memory(text: &str) -> Result<(u64, u64)> {
    let kib = |key: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    let total = kib("MemTotal").ok_or_else(|| invalid("no MemTotal in meminfo"))?;
    let available = kib("MemAvailable").ok_or_else(|| invalid("no MemAvailable in meminfo"))?;
    Ok((
        total.saturating_mul(BYTES_PER_KIB),
        available.min(total).saturating_mul(BYTES_PER_KIB),
    ))
}

/// Processes are the numeric directories of `/proc`. One that exits while
/// the directory is read simply is not counted.
fn count_processes(proc_root: &Path) -> Result<u32> {
    let count = std::fs::read_dir(proc_root)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
        })
        .count();
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKS: u64 = 100;
    const NANOS_PER_TICK: u64 = NANOS_PER_SECOND / TICKS;

    #[rstest::rstest]
    #[case::two_cpus(
        "cpu  10 2 5 900 7 1 1 1 4 0\ncpu0 5 1 2 450 3 0 1 0 0 0\ncpu1 5 1 3 450 4 1 0 1 0 0\n",
        // user + nice + system + irq + softirq + steal, no idle or iowait
        (10 + 2 + 5 + 1 + 1 + 1) * NANOS_PER_TICK,
        2
    )]
    fn given_proc_stat_when_parsed_then_busy_time_and_cpu_count(
        #[case] text: &str,
        #[case] nanos: u64,
        #[case] cpus: u32,
    ) {
        assert_eq!(parse_cpu(text, TICKS).expect("parse"), (nanos, cpus));
    }

    #[test]
    fn given_a_proc_tree_when_sampled_then_only_numeric_directories_count_as_processes() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("stat"), "cpu 1 0 0 0\ncpu0 1 0 0 0\n").unwrap();
        std::fs::write(
            dir.path().join("meminfo"),
            "MemTotal: 8 kB\nMemAvailable: 4 kB\n",
        )
        .unwrap();
        for entry in ["1", "42", "self", "sys", "4x"] {
            std::fs::create_dir(dir.path().join(entry)).unwrap();
        }

        // Act
        let sample = sample(dir.path()).expect("sample");

        // Assert
        assert_eq!(sample.pids, 2);
        assert_eq!(sample.online_cpus, 1);
        assert_eq!(sample.mem_total_bytes, 8 * BYTES_PER_KIB);
        assert_eq!(sample.mem_available_bytes, 4 * BYTES_PER_KIB);
    }
}
