// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Facts about the host the daemon runs on, for `docker info`.

use std::path::Path;
use std::process::Command;

const MEMINFO_PATH: &str = "/proc/meminfo";
const MEMINFO_TOTAL_KEY: &str = "MemTotal:";
const BYTES_PER_KIB: u64 = 1024;
const OS_RELEASE_PATH: &str = "/etc/os-release";
const OS_RELEASE_NAME_KEY: &str = "PRETTY_NAME=";
const MACOS_NAME: &str = "macOS";
const DAEMON_ID_FILE: &str = "daemon-id";

/// What the daemon reports about its host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostInfo {
    pub hostname: String,
    pub kernel_version: String,
    pub operating_system: String,
    pub ncpu: u32,
    pub mem_total_bytes: u64,
}

impl HostInfo {
    pub fn detect() -> Self {
        let uname = rustix::system::uname();
        let lossy = |name: &std::ffi::CStr| name.to_string_lossy().into_owned();
        let sysname = lossy(uname.sysname());
        Self {
            hostname: lossy(uname.nodename()),
            kernel_version: lossy(uname.release()),
            operating_system: operating_system(&sysname),
            ncpu: std::thread::available_parallelism()
                .map_or(0, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
            mem_total_bytes: mem_total_bytes(),
        }
    }
}

/// `MemTotal` of a `/proc/meminfo` text, in bytes.
fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with(MEMINFO_TOTAL_KEY))?;
    let kib: u64 = line
        .trim_start_matches(MEMINFO_TOTAL_KEY)
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kib.checked_mul(BYTES_PER_KIB)
}

/// The quoted `PRETTY_NAME` of an `os-release` text.
fn parse_os_release(text: &str) -> Option<String> {
    let value = text
        .lines()
        .find_map(|l| l.strip_prefix(OS_RELEASE_NAME_KEY))?;
    Some(value.trim_matches(['"', '\'']).to_string())
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn mem_total_bytes() -> u64 {
    if let Ok(text) = std::fs::read_to_string(MEMINFO_PATH) {
        return parse_meminfo(&text).unwrap_or(0);
    }
    command_output("sysctl", &["-n", "hw.memsize"])
        .and_then(|bytes| bytes.parse().ok())
        .unwrap_or(0)
}

fn operating_system(sysname: &str) -> String {
    if let Some(name) = std::fs::read_to_string(OS_RELEASE_PATH)
        .ok()
        .as_deref()
        .and_then(parse_os_release)
    {
        return name;
    }
    match command_output("sw_vers", &["-productVersion"]) {
        Some(version) => format!("{MACOS_NAME} {version}"),
        None => sysname.to_string(),
    }
}

/// The id stored in `data_dir`, created on first use. Empty, with a
/// warning in the log, when the directory cannot be written.
pub fn daemon_id(data_dir: &Path) -> String {
    use std::io::Write;

    let path = data_dir.join(DAEMON_ID_FILE);
    let read = || {
        std::fs::read_to_string(&path)
            .map(|text| text.trim().to_string())
            .ok()
            .filter(|id| !id.is_empty())
    };
    if let Some(existing) = read() {
        return existing;
    }
    let id = uuid::Uuid::new_v4().to_string();
    // create_new makes two first callers agree: the loser reads the winner's id.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(id.as_bytes()));
    match written {
        Ok(()) => id,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read().unwrap_or_default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot store the daemon id");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_meminfo_when_parsed_then_total_is_in_bytes() {
        let text = "MemFree: 5 kB\nMemTotal:       16384 kB\n";

        assert_eq!(parse_meminfo(text), Some(16_384 * 1024));
    }

    #[test]
    fn given_no_total_when_meminfo_parsed_then_none() {
        assert_eq!(parse_meminfo("MemFree: 5 kB\n"), None);
    }

    #[test]
    fn given_os_release_when_parsed_then_pretty_name_loses_its_quotes() {
        let text = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n";

        assert_eq!(parse_os_release(text).as_deref(), Some("Ubuntu 24.04 LTS"));
    }

    #[test]
    fn given_a_data_dir_when_id_asked_twice_then_it_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir");

        let first = daemon_id(dir.path());
        let second = daemon_id(dir.path());

        assert!(!first.is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn given_this_host_when_detected_then_it_has_cpus_and_an_operating_system() {
        let host = HostInfo::detect();

        assert!(host.ncpu > 0);
        assert!(!host.operating_system.is_empty());
    }

    #[test]
    fn given_a_missing_data_dir_when_id_asked_then_it_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert_eq!(daemon_id(&dir.path().join("absent")), "");
    }

    #[test]
    fn given_an_id_file_when_id_asked_then_the_stored_id_is_returned() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(DAEMON_ID_FILE), "fixed-id\n").expect("write");

        assert_eq!(daemon_id(dir.path()), "fixed-id");
    }

    #[rstest::rstest]
    #[case::single_quotes("PRETTY_NAME='Alpine Linux'\n", Some("Alpine Linux"))]
    #[case::unquoted("PRETTY_NAME=Arch\n", Some("Arch"))]
    #[case::missing("NAME=Debian\n", None)]
    fn given_os_release_variants_when_parsed_then_quotes_are_dropped_and_absence_is_none(
        #[case] text: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(parse_os_release(text).as_deref(), expected);
    }

    #[test]
    fn given_an_overflowing_meminfo_when_parsed_then_none() {
        assert_eq!(parse_meminfo("MemTotal: 18446744073709551615 kB\n"), None);
    }
}
