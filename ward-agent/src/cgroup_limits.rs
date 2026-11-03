// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! CPU and memory settings of the sandbox cgroup: `cpu.weight`, `cpu.max`,
//! `cpuset.cpus` and `memory.low`. A setting the guest kernel cannot apply is
//! an error, never skipped.

use std::path::Path;

use crate::cgroup::{CONTROLLERS_FILE, SUBTREE_CONTROL_FILE, write_file};

/// Argument for `memory.low`: `--memory-low <bytes>`.
pub const MEMORY_LOW_ARG: &str = "--memory-low";
/// Argument for `cpu.weight`: `--cpu-weight <weight>`.
pub const CPU_WEIGHT_ARG: &str = "--cpu-weight";
/// Argument for `cpu.max`: `--cpu-max <quota|max> <period>`.
pub const CPU_MAX_ARG: &str = "--cpu-max";
/// Argument for `cpuset.cpus`: `--cpuset-cpus <list>`.
pub const CPUSET_ARG: &str = "--cpuset-cpus";

const CPU_CONTROLLER: &str = "cpu";
const CPUSET_CONTROLLER: &str = "cpuset";
const CPU_WEIGHT_FILE: &str = "cpu.weight";
const CPU_MAX_FILE: &str = "cpu.max";
const CPUSET_FILE: &str = "cpuset.cpus";
const MEMORY_LOW_FILE: &str = "memory.low";

/// What to set on the sandbox cgroup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupSettings {
    pub memory_low: Option<u64>,
    pub cpu_weight: Option<u64>,
    /// Quota in microseconds, or `max`, and the period.
    pub cpu_max: Option<(String, u64)>,
    pub cpuset_cpus: Option<String>,
}

impl CgroupSettings {
    /// True when nothing is asked for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Apply `settings` to the cgroup at `group`, below `root`.
///
/// # Errors
///
/// Fails with the setting and the reason when the kernel lacks the
/// controller or refuses the value.
pub fn apply(root: &Path, group: &Path, settings: &CgroupSettings) -> Result<(), String> {
    let controllers = std::fs::read_to_string(root.join(CONTROLLERS_FILE))
        .map_err(|e| format!("no cgroup v2 controllers: {e}"))?;
    let has = |name: &str| controllers.split_whitespace().any(|c| c == name);
    let enable = |name: &str| -> Result<(), String> {
        if !has(name) {
            return Err(format!("the guest kernel has no {name} controller"));
        }
        write_file(&root.join(SUBTREE_CONTROL_FILE), &format!("+{name}"))
            .map_err(|e| format!("cannot enable the {name} controller: {e}"))
    };
    let set = |file: &str, value: &str| -> Result<(), String> {
        write_file(&group.join(file), value).map_err(|e| format!("cannot write {file}: {e}"))
    };

    if settings.cpu_weight.is_some() || settings.cpu_max.is_some() {
        enable(CPU_CONTROLLER)?;
    }
    if settings.cpuset_cpus.is_some() {
        enable(CPUSET_CONTROLLER)?;
    }
    if let Some(weight) = settings.cpu_weight {
        set(CPU_WEIGHT_FILE, &weight.to_string())?;
    }
    if let Some((quota, period)) = &settings.cpu_max {
        set(CPU_MAX_FILE, &format!("{quota} {period}"))?;
    }
    if let Some(cpus) = &settings.cpuset_cpus {
        set(CPUSET_FILE, cpus)?;
    }
    if let Some(bytes) = settings.memory_low {
        set(MEMORY_LOW_FILE, &bytes.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cgroup root with the given controllers and the group files the
    /// kernel would create.
    fn fake(controllers: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(CONTROLLERS_FILE), controllers).unwrap();
        std::fs::write(root.path().join(SUBTREE_CONTROL_FILE), "").unwrap();
        let group = root.path().join("ward");
        std::fs::create_dir(&group).unwrap();
        for file in [CPU_WEIGHT_FILE, CPU_MAX_FILE, CPUSET_FILE, MEMORY_LOW_FILE] {
            std::fs::write(group.join(file), "").unwrap();
        }
        root
    }

    fn read(root: &tempfile::TempDir, file: &str) -> String {
        std::fs::read_to_string(root.path().join("ward").join(file)).unwrap()
    }

    #[test]
    fn given_settings_when_applied_then_each_file_gets_its_value() {
        // Arrange
        let root = fake("cpu cpuset memory pids");
        let settings = CgroupSettings {
            memory_low: Some(4096),
            cpu_weight: Some(39),
            cpu_max: Some(("50000".into(), 100_000)),
            cpuset_cpus: Some("0-1".into()),
        };

        // Act
        apply(root.path(), &root.path().join("ward"), &settings).unwrap();

        // Assert
        assert_eq!(read(&root, CPU_WEIGHT_FILE), "39");
        assert_eq!(read(&root, CPU_MAX_FILE), "50000 100000");
        assert_eq!(read(&root, CPUSET_FILE), "0-1");
        assert_eq!(read(&root, MEMORY_LOW_FILE), "4096");
    }

    #[test]
    fn given_a_missing_controller_when_applied_then_it_fails_and_names_it() {
        // Arrange
        let root = fake("memory pids");
        let settings = CgroupSettings {
            cpu_weight: Some(10),
            ..Default::default()
        };

        // Act
        let error = apply(root.path(), &root.path().join("ward"), &settings).unwrap_err();

        // Assert
        assert!(error.contains("no cpu controller"), "{error}");
    }

    #[test]
    fn given_nothing_to_set_when_applied_then_it_is_a_no_op() {
        let root = fake("");

        assert!(CgroupSettings::default().is_empty());
        assert!(
            apply(
                root.path(),
                &root.path().join("ward"),
                &CgroupSettings::default()
            )
            .is_ok()
        );
    }
}
