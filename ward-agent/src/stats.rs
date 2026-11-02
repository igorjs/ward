// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest resource counters read from `/proc` and the sandbox cgroup.

use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::path::Path;
use std::sync::Mutex;
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
const PROC_READ_CHARS_KEY: &str = "rchar";
const PROC_WRITE_CHARS_KEY: &str = "wchar";
const CGROUP_PROCS_FILE: &str = "cgroup.procs";

fn invalid(what: &str) -> Error {
    Error::new(ErrorKind::InvalidData, what.to_string())
}

/// Syscall-level I/O of the sandbox processes: the last reading of each
/// live one, plus the final reading of every one that has exited.
#[derive(Default)]
pub struct IoLedger {
    live: HashMap<u32, (u64, u64)>,
    retired: (u64, u64),
}

impl IoLedger {
    /// Take a fresh reading of every sandbox process. A process that is gone
    /// (or whose counters went backwards, a reused pid) keeps its last
    /// reading in the retired total.
    pub fn update(&mut self, proc_root: &Path, group: Option<&Path>) {
        let now = read_process_io(proc_root, group);
        for (pid, last) in &self.live {
            if now
                .get(pid)
                .is_none_or(|cur| cur.0 < last.0 || cur.1 < last.1)
            {
                self.retired = add(self.retired, *last);
            }
        }
        self.live = now;
    }

    fn total(&self) -> (u64, u64) {
        self.live
            .values()
            .fold(self.retired, |sum, io| add(sum, *io))
    }
}

fn add(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (a.0.saturating_add(b.0), a.1.saturating_add(b.1))
}

/// Read one sample from the `/proc` tree at `proc_root`. `group` is the
/// sandbox cgroup, when there is one; `peak` holds the highest memory use
/// seen across samples, for guests whose cgroup has no `memory.peak`, and
/// `io` the process I/O used when the cgroup reports no block I/O.
pub fn sample(
    proc_root: &Path,
    group: Option<&Path>,
    peak: &AtomicU64,
    io: &Mutex<IoLedger>,
) -> Result<StatsSample> {
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
    let (block_read_bytes, block_write_bytes) = block_io(proc_root, group, io);
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

/// Bytes read and written: the cgroup's `io.stat` when it has data, else
/// what the sandbox processes passed to read and write calls. On virtiofs
/// and tmpfs no block I/O happens, so `io.stat` stays empty and only the
/// syscall counts show activity.
fn block_io(proc_root: &Path, group: Option<&Path>, io: &Mutex<IoLedger>) -> (u64, u64) {
    let from_cgroup = group
        .and_then(|dir| std::fs::read_to_string(dir.join(IO_STAT_FILE)).ok())
        .map(|text| sum_io_stat(&text))
        .filter(|&(read, written)| read != 0 || written != 0);
    from_cgroup.unwrap_or_else(|| {
        let mut ledger = io.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.update(proc_root, group);
        ledger.total()
    })
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

/// The pids in the sandbox cgroup, or every process when there is none.
fn sandbox_pids(proc_root: &Path, group: Option<&Path>) -> Vec<u32> {
    let listed = group
        .and_then(|dir| std::fs::read_to_string(dir.join(CGROUP_PROCS_FILE)).ok())
        .map(|text| {
            text.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect()
        });
    listed.unwrap_or_else(|| {
        std::fs::read_dir(proc_root)
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .filter(|entry| is_pid_name(&entry.file_name()))
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    })
}

fn read_process_io(proc_root: &Path, group: Option<&Path>) -> HashMap<u32, (u64, u64)> {
    sandbox_pids(proc_root, group)
        .into_iter()
        .filter_map(|pid| {
            let text =
                std::fs::read_to_string(proc_root.join(pid.to_string()).join(PROC_IO_FILE)).ok()?;
            Some((pid, parse_proc_io(&text)))
        })
        .collect()
}

/// `rchar` and `wchar` from one `/proc/<pid>/io`.
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
    (value(PROC_READ_CHARS_KEY), value(PROC_WRITE_CHARS_KEY))
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
        let sample =
            sample(dir.path(), None, &AtomicU64::new(0), &Mutex::default()).expect("sample");

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
    fn given_proc_io_when_parsed_then_syscall_characters_not_storage_bytes() {
        let text = "rchar: 9\nwchar: 7\nsyscr: 1\nsyscw: 1\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";

        assert_eq!(parse_proc_io(text), (9, 7));
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
            ("1", "rchar: 10\nwchar: 20\n"),
            ("2", "rchar: 1\nwchar: 2\n"),
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
        let sample = sample(
            proc.path(),
            Some(group.path()),
            &AtomicU64::new(0),
            &Mutex::default(),
        )
        .unwrap();

        // Assert
        assert_eq!(sample.block_read_bytes, 101);
        assert_eq!(sample.block_write_bytes, 2020);
        assert_eq!(sample.mem_peak_bytes, 12345);
    }

    #[test]
    fn given_no_io_stat_when_sampled_then_block_bytes_sum_the_process_characters() {
        // Arrange
        let proc = proc_tree();
        let group = tempfile::tempdir().unwrap();

        // Act
        let sample = sample(
            proc.path(),
            Some(group.path()),
            &AtomicU64::new(0),
            &Mutex::default(),
        )
        .unwrap();

        // Assert
        assert_eq!(sample.block_read_bytes, 11);
        assert_eq!(sample.block_write_bytes, 22);
    }

    #[test]
    fn given_io_stat_with_zero_bytes_when_sampled_then_process_characters_are_used() {
        // Arrange
        let proc = proc_tree();
        let group = tempfile::tempdir().unwrap();
        std::fs::write(
            group.path().join(IO_STAT_FILE),
            "8:0 rbytes=0 wbytes=0 rios=0 wios=0\n",
        )
        .unwrap();

        // Act
        let sample = sample(
            proc.path(),
            Some(group.path()),
            &AtomicU64::new(0),
            &Mutex::default(),
        )
        .unwrap();

        // Assert
        assert_eq!(
            (sample.block_read_bytes, sample.block_write_bytes),
            (11, 22)
        );
    }

    #[test]
    fn given_a_cgroup_with_procs_when_sampled_then_only_its_processes_count() {
        // Arrange
        let proc = proc_tree();
        let group = tempfile::tempdir().unwrap();
        std::fs::write(group.path().join(CGROUP_PROCS_FILE), "2\n").unwrap();

        // Act
        let sample = sample(
            proc.path(),
            Some(group.path()),
            &AtomicU64::new(0),
            &Mutex::default(),
        )
        .unwrap();

        // Assert
        assert_eq!((sample.block_read_bytes, sample.block_write_bytes), (1, 2));
    }

    #[test]
    fn given_a_process_that_exited_when_sampled_again_then_its_last_reading_is_kept() {
        // Arrange
        let proc = proc_tree();
        let ledger = Mutex::default();
        let peak = AtomicU64::new(0);
        sample(proc.path(), None, &peak, &ledger).unwrap();
        std::fs::remove_dir_all(proc.path().join("1")).unwrap();
        std::fs::write(
            proc.path().join("2").join(PROC_IO_FILE),
            "rchar: 5\nwchar: 6\n",
        )
        .unwrap();

        // Act
        let sample = sample(proc.path(), None, &peak, &ledger).unwrap();

        // Assert
        assert_eq!(
            (sample.block_read_bytes, sample.block_write_bytes),
            (15, 26)
        );
    }

    #[test]
    fn given_no_memory_peak_file_when_sampled_then_peak_is_the_highest_use_seen() {
        // Arrange
        let proc = proc_tree();
        let tracked = AtomicU64::new(100 * BYTES_PER_KIB);

        // Act
        let sample = sample(proc.path(), None, &tracked, &Mutex::default()).unwrap();

        // Assert
        assert_eq!(sample.mem_peak_bytes, 100 * BYTES_PER_KIB);
    }
}
