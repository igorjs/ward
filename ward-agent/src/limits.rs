// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Changes the memory, CPU and pids limits of the running sandbox cgroup.

use std::io;
use std::path::Path;

const CONTROLLERS_FILE: &str = "cgroup.controllers";
const SUBTREE_CONTROL_FILE: &str = "cgroup.subtree_control";
const MEMORY_MAX_FILE: &str = "memory.max";
const CPU_MAX_FILE: &str = "cpu.max";
const PIDS_MAX_FILE: &str = "pids.max";
const CPU_CONTROLLER: &str = "cpu";
const PIDS_CONTROLLER: &str = "pids";
/// What the cgroup files hold for "no limit".
const UNLIMITED: &str = "max";
/// The scheduling period `cpu.max` quotas are measured against.
const CPU_PERIOD_MICROS: u128 = 100_000;
const NANOS_PER_CPU: u128 = 1_000_000_000;
/// The smallest quota the kernel accepts.
const MIN_CPU_QUOTA_MICROS: u128 = 1_000;

/// The limits to change. A field left `None` keeps the cgroup as it is, and
/// 0 removes that limit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: Option<u64>,
    pub nano_cpus: Option<u64>,
    pub pids_max: Option<u32>,
}

/// Write `limits` into the cgroup at `dir`. Every needed controller is
/// checked before the first write, so a refusal changes nothing.
///
/// # Errors
///
/// Fails when the guest kernel lacks the controller a limit needs, or the
/// kernel refuses a write.
pub fn apply(dir: &Path, limits: Limits) -> io::Result<()> {
    let root = dir.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the sandbox cgroup has no parent",
        )
    })?;
    if limits.memory_bytes.is_some() && !dir.join(MEMORY_MAX_FILE).exists() {
        return Err(unsupported("the guest kernel has no memory controller"));
    }
    let available = std::fs::read_to_string(root.join(CONTROLLERS_FILE))?;
    let has = |name: &str| available.split_whitespace().any(|c| c == name);
    if limits.nano_cpus.is_some() && !has(CPU_CONTROLLER) {
        return Err(unsupported("the guest kernel has no cpu controller"));
    }
    if limits.pids_max.is_some() && !has(PIDS_CONTROLLER) {
        return Err(unsupported("the guest kernel has no pids controller"));
    }

    if limits.nano_cpus.is_some() {
        enable(root, CPU_CONTROLLER)?;
    }
    if limits.pids_max.is_some() {
        enable(root, PIDS_CONTROLLER)?;
    }

    let mut writes = Vec::new();
    if let Some(bytes) = limits.memory_bytes {
        writes.push((dir.join(MEMORY_MAX_FILE), limit_text(bytes)));
    }
    if let Some(nanos) = limits.nano_cpus {
        writes.push((dir.join(CPU_MAX_FILE), cpu_max_text(nanos)));
    }
    if let Some(pids) = limits.pids_max {
        writes.push((dir.join(PIDS_MAX_FILE), limit_text(u64::from(pids))));
    }
    write_all_or_none(&writes)
}

/// Write every file, or put back the ones already written when a later write
/// fails. The old contents are read before the first write.
fn write_all_or_none(writes: &[(std::path::PathBuf, String)]) -> io::Result<()> {
    let old = writes
        .iter()
        .map(|(path, _)| std::fs::read_to_string(path))
        .collect::<io::Result<Vec<_>>>()?;
    for (done, (path, text)) in writes.iter().enumerate() {
        if let Err(e) = std::fs::write(path, text) {
            for ((path, _), before) in writes.iter().zip(&old).take(done) {
                // Best effort: the kernel already accepted these values once.
                let _ = std::fs::write(path, before.trim());
            }
            return Err(e);
        }
    }
    Ok(())
}

fn unsupported(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, why)
}

fn limit_text(value: u64) -> String {
    if value == 0 {
        UNLIMITED.to_string()
    } else {
        value.to_string()
    }
}

fn cpu_max_text(nano_cpus: u64) -> String {
    if nano_cpus == 0 {
        return format!("{UNLIMITED} {CPU_PERIOD_MICROS}");
    }
    let quota =
        (u128::from(nano_cpus) * CPU_PERIOD_MICROS / NANOS_PER_CPU).max(MIN_CPU_QUOTA_MICROS);
    format!("{quota} {CPU_PERIOD_MICROS}")
}

/// Turn `controller` on for the cgroups below the root, unless it already is.
fn enable(root: &Path, controller: &str) -> io::Result<()> {
    let enabled = std::fs::read_to_string(root.join(SUBTREE_CONTROL_FILE))?;
    if enabled.split_whitespace().any(|c| c == controller) {
        return Ok(());
    }
    std::fs::write(root.join(SUBTREE_CONTROL_FILE), format!("+{controller}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: &str = "ward";

    /// A fake cgroup root with the files the kernel would provide.
    fn fake_root(controllers: &str, memory: bool) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(CONTROLLERS_FILE), controllers).unwrap();
        std::fs::write(root.path().join(SUBTREE_CONTROL_FILE), "memory").unwrap();
        let group = root.path().join(GROUP);
        std::fs::create_dir(&group).unwrap();
        for file in [CPU_MAX_FILE, PIDS_MAX_FILE] {
            std::fs::write(group.join(file), UNLIMITED).unwrap();
        }
        if memory {
            std::fs::write(group.join(MEMORY_MAX_FILE), UNLIMITED).unwrap();
        }
        root
    }

    fn read(root: &tempfile::TempDir, file: &str) -> String {
        std::fs::read_to_string(root.path().join(GROUP).join(file)).unwrap()
    }

    #[test]
    fn given_all_controllers_when_limits_applied_then_each_file_holds_its_value() {
        // Arrange
        let root = fake_root("cpu memory pids", true);
        let limits = Limits {
            memory_bytes: Some(64 * 1024 * 1024),
            nano_cpus: Some(500_000_000),
            pids_max: Some(32),
        };

        // Act
        apply(&root.path().join(GROUP), limits).unwrap();

        // Assert
        assert_eq!(read(&root, MEMORY_MAX_FILE), "67108864");
        assert_eq!(read(&root, CPU_MAX_FILE), "50000 100000");
        assert_eq!(read(&root, PIDS_MAX_FILE), "32");
    }

    #[test]
    fn given_zero_limits_when_applied_then_files_hold_max() {
        // Arrange
        let root = fake_root("cpu memory pids", true);
        let limits = Limits {
            memory_bytes: Some(0),
            nano_cpus: Some(0),
            pids_max: Some(0),
        };

        // Act
        apply(&root.path().join(GROUP), limits).unwrap();

        // Assert
        assert_eq!(read(&root, MEMORY_MAX_FILE), "max");
        assert_eq!(read(&root, CPU_MAX_FILE), "max 100000");
        assert_eq!(read(&root, PIDS_MAX_FILE), "max");
    }

    #[test]
    fn given_unset_fields_when_applied_then_their_files_are_untouched() {
        // Arrange
        let root = fake_root("cpu memory pids", true);
        let limits = Limits {
            pids_max: Some(7),
            ..Limits::default()
        };

        // Act
        apply(&root.path().join(GROUP), limits).unwrap();

        // Assert
        assert_eq!(read(&root, MEMORY_MAX_FILE), "max");
        assert_eq!(read(&root, CPU_MAX_FILE), "max");
    }

    #[test]
    fn given_tiny_cpu_limit_when_applied_then_quota_is_the_kernel_minimum() {
        // Arrange
        let root = fake_root("cpu", false);
        let limits = Limits {
            nano_cpus: Some(1),
            ..Limits::default()
        };

        // Act
        apply(&root.path().join(GROUP), limits).unwrap();

        // Assert
        assert_eq!(read(&root, CPU_MAX_FILE), "1000 100000");
    }

    #[test]
    fn given_missing_controller_when_applied_then_nothing_is_written() {
        // Arrange
        let root = fake_root("memory", true);
        let limits = Limits {
            memory_bytes: Some(1024),
            pids_max: Some(5),
            ..Limits::default()
        };

        // Act
        let err = apply(&root.path().join(GROUP), limits).unwrap_err();

        // Assert
        assert!(err.to_string().contains("pids controller"), "{err}");
        assert_eq!(read(&root, MEMORY_MAX_FILE), "max");
    }

    #[test]
    fn given_no_memory_controller_when_memory_limited_then_it_is_unsupported() {
        // Arrange
        let root = fake_root("cpu pids", false);
        let limits = Limits {
            memory_bytes: Some(1024),
            ..Limits::default()
        };

        // Act
        let err = apply(&root.path().join(GROUP), limits).unwrap_err();

        // Assert
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn given_a_later_write_that_fails_when_applied_then_earlier_files_are_restored() {
        // Arrange
        let root = fake_root("cpu memory pids", true);
        let group = root.path().join(GROUP);
        std::fs::write(group.join(MEMORY_MAX_FILE), "1000").unwrap();
        // A read-only file reads fine but cannot be written, so the pids
        // write fails last.
        let mut mode = std::fs::metadata(group.join(PIDS_MAX_FILE))
            .unwrap()
            .permissions();
        mode.set_readonly(true);
        std::fs::set_permissions(group.join(PIDS_MAX_FILE), mode).unwrap();
        let limits = Limits {
            memory_bytes: Some(2000),
            nano_cpus: Some(500_000_000),
            pids_max: Some(5),
        };

        // Act
        let result = apply(&group, limits);

        // Assert
        assert!(result.is_err());
        assert_eq!(read(&root, MEMORY_MAX_FILE), "1000");
        assert_eq!(read(&root, CPU_MAX_FILE), "max");
    }

    #[test]
    fn given_controller_off_for_children_when_limited_then_it_is_enabled_first() {
        // Arrange
        let root = fake_root("cpu pids", false);
        let limits = Limits {
            pids_max: Some(5),
            ..Limits::default()
        };

        // Act
        apply(&root.path().join(GROUP), limits).unwrap();

        // Assert
        let enabled = std::fs::read_to_string(root.path().join(SUBTREE_CONTROL_FILE)).unwrap();
        assert_eq!(enabled, "+pids");
    }
}
