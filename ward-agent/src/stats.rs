// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest resource counters read from `/proc` and the sandbox cgroup.

use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::proto::StatsSample;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const BYTES_PER_KIB: u64 = 1024;
/// Used when the kernel reports no clock tick rate.
const FALLBACK_TICKS_PER_SECOND: u64 = 100;

/// Indexes into the aggregate `cpu` line of `/proc/stat`: user, nice,
/// system, irq, softirq and steal. Idle, iowait and the guest columns (the
/// kernel counts those inside user and nice) are left out.
const CPU_BUSY_FIELDS: [usize; 6] = [0, 1, 2, 5, 6, 7];

const IO_STAT_FILE: &str = "io.stat";
const MEMORY_PEAK_FILE: &str = "memory.peak";
const PROC_IO_FILE: &str = "io";
const READ_BYTES_KEY: &str = "rbytes";
const WRITE_BYTES_KEY: &str = "wbytes";
const PROC_READ_BYTES_KEY: &str = "read_bytes";
const PROC_WRITE_BYTES_KEY: &str = "write_bytes";

fn invalid(what: &str) -> Error {
    Error::new(ErrorKind::InvalidData, what.to_string())
}

/// Read one sample from the `/proc` tree at `proc_root`. `group` is the
/// sandbox cgroup, when there is one; `peak` holds the highest memory use
/// seen across samples, for guests whose cgroup has no `memory.peak`.
pub fn sample(proc_root: &Path, group: Option<&Path>, peak: &AtomicU64) -> Result<StatsSample> {
    let ticks = match rustix::param::clock_ticks_per_second() {
        0 => FALLBACK_TICKS_PER_SECOND,
        ticks => ticks,
    };
    let (cpu_nanos, online_cpus) =
        parse_cpu(&std::fs::read_to_string(proc_root.join("stat"))?, ticks)?;
    let (mem_total_bytes, mem_available_bytes) =
        parse_memory(&std::fs::read_to_string(proc_root.join("meminfo"))?)?;
    let used = mem_total_bytes.saturating_sub(mem_available_bytes);
    let tracked_peak = peak.fetch_max(used, Ordering::Relaxed).max(used);
    let (block_read_bytes, block_write_bytes) = block_io(proc_root, group);
    Ok(StatsSample {
        cpu_nanos,
        online_cpus,
        mem_total_bytes,
        mem_available_bytes,
        pids: count_processes(proc_root)?,
        mem_peak_bytes: group
            .and_then(|dir| read_number(&dir.join(MEMORY_PEAK_FILE)))
            .unwrap_or(tracked_peak),
        block_read_bytes,
        block_write_bytes,
    })
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Bytes read and written on block devices: the cgroup's `io.stat` when its
/// io controller is on, else the sum of `/proc/<pid>/io` over the processes
/// alive now (the counters of exited ones are gone).
fn block_io(proc_root: &Path, group: Option<&Path>) -> (u64, u64) {
    let from_cgroup = group
        .and_then(|dir| std::fs::read_to_string(dir.join(IO_STAT_FILE)).ok())
        .map(|text| sum_io_stat(&text));
    from_cgroup.unwrap_or_else(|| sum_process_io(proc_root))
}

/// Sum the `rbytes` and `wbytes` of every device line of a cgroup `io.stat`.
fn sum_io_stat(text: &str) -> (u64, u64) {
    let mut total = (0u64, 0u64);
    for pair in text.lines().flat_map(str::split_whitespace) {
        match pair.split_once('=') {
            Some((READ_BYTES_KEY, n)) => total.0 = total.0.saturating_add(n.parse().unwrap_or(0)),
            Some((WRITE_BYTES_KEY, n)) => total.1 = total.1.saturating_add(n.parse().unwrap_or(0)),
            _ => {}
        }
    }
    total
}

fn sum_process_io(proc_root: &Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return (0, 0);
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| is_pid_name(&entry.file_name()))
        .filter_map(|entry| std::fs::read_to_string(entry.path().join(PROC_IO_FILE)).ok())
        .map(|text| parse_proc_io(&text))
        .fold((0, 0), |(r, w), (pr, pw)| {
            (r.saturating_add(pr), w.saturating_add(pw))
        })
}

/// `read_bytes` and `write_bytes` from one `/proc/<pid>/io`.
fn parse_proc_io(text: &str) -> (u64, u64) {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| {
                line.strip_prefix(key)?
                    .strip_prefix(':')?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or(0)
    };
    (value(PROC_READ_BYTES_KEY), value(PROC_WRITE_BYTES_KEY))
}

fn is_pid_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
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
        .filter(|entry| is_pid_name(&entry.file_name()))
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
        let sample = sample(dir.path(), None, &AtomicU64::new(0)).expect("sample");

        // Assert
        assert_eq!(sample.pids, 2);
        assert_eq!(sample.online_cpus, 1);
        assert_eq!(sample.mem_total_bytes, 8 * BYTES_PER_KIB);
        assert_eq!(sample.mem_available_bytes, 4 * BYTES_PER_KIB);
    }

    const IO_STAT: &str = "8:0 rbytes=100 wbytes=2000 rios=3 wios=4 dbytes=0 dios=0\n\
                           8:16 rbytes=1 wbytes=20 rios=1 wios=1 dbytes=0 dios=0\n";

    #[test]
    fn given_io_stat_with_two_devices_when_summed_then_bytes_add_across_devices() {
        assert_eq!(sum_io_stat(IO_STAT), (101, 2020));
    }

    #[test]
    fn given_proc_io_when_parsed_then_storage_bytes_not_characters() {
        let text = "rchar: 9\nwchar: 9\nsyscr: 1\nsyscw: 1\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";

        assert_eq!(parse_proc_io(text), (4096, 8192));
    }

    fn proc_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("stat"), "cpu 1 0 0 0\ncpu0 1 0 0 0\n").unwrap();
        std::fs::write(
            dir.path().join("meminfo"),
            "MemTotal: 8 kB\nMemAvailable: 4 kB\n",
        )
        .unwrap();
        for (pid, io) in [
            ("1", "read_bytes: 10\nwrite_bytes: 20\n"),
            ("2", "read_bytes: 1\nwrite_bytes: 2\n"),
        ] {
            std::fs::create_dir(dir.path().join(pid)).unwrap();
            std::fs::write(dir.path().join(pid).join(PROC_IO_FILE), io).unwrap();
        }
        dir
    }

    #[test]
    fn given_a_cgroup_with_io_stat_when_sampled_then_block_bytes_come_from_the_cgroup() {
        // Arrange
        let proc = proc_tree();
        let group = tempfile::tempdir().unwrap();
        std::fs::write(group.path().join(IO_STAT_FILE), IO_STAT).unwrap();
        std::fs::write(group.path().join(MEMORY_PEAK_FILE), "12345\n").unwrap();

        // Act
        let sample = sample(proc.path(), Some(group.path()), &AtomicU64::new(0)).unwrap();

        // Assert
        assert_eq!(sample.block_read_bytes, 101);
        assert_eq!(sample.block_write_bytes, 2020);
        assert_eq!(sample.mem_peak_bytes, 12345);
    }

    #[test]
    fn given_no_io_controller_when_sampled_then_block_bytes_sum_the_processes() {
        // Arrange
        let proc = proc_tree();
        let group = tempfile::tempdir().unwrap();

        // Act
        let sample = sample(proc.path(), Some(group.path()), &AtomicU64::new(0)).unwrap();

        // Assert
        assert_eq!(sample.block_read_bytes, 11);
        assert_eq!(sample.block_write_bytes, 22);
    }

    #[test]
    fn given_no_memory_peak_file_when_sampled_then_peak_is_the_highest_use_seen() {
        // Arrange
        let proc = proc_tree();
        let tracked = AtomicU64::new(100 * BYTES_PER_KIB);

        // Act
        let sample = sample(proc.path(), None, &tracked).unwrap();

        // Assert
        assert_eq!(sample.mem_peak_bytes, 100 * BYTES_PER_KIB);
    }
}
