// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `--log-driver` and `--log-opt` of `ward run` and `ward create`.
//!
//! Ward keeps each sandbox's output itself and has no log drivers. The two
//! drivers that match what it does, `json-file` and `local`, are accepted;
//! any other driver is refused.

use anyhow::{Result, bail};
use clap::Args;

const DOCS: &str = "https://github.com/igorjs/ward/blob/main/docs/docker-differences.md";
/// The drivers whose behavior Ward already has: the output is kept and
/// `ward logs` reads it.
const IGNORED_OPTS_WARNING: &str =
    "warning: --log-opt is ignored: Ward does not rotate or tag sandbox output";
const ACCEPTED_DRIVERS: [&str; 2] = ["json-file", "local"];

#[derive(Debug, Args, Default)]
pub struct LogFlags {
    /// Accepted for json-file and local, which is what Ward does anyway. Any
    /// other driver is refused.
    #[arg(long = "log-driver", value_name = "DRIVER")]
    log_driver: Option<String>,
    /// Accepted and ignored: Ward does not rotate or tag sandbox output.
    /// Repeatable.
    #[arg(long = "log-opt", value_name = "KEY=VALUE")]
    log_opt: Vec<String>,
}

impl LogFlags {
    /// Refuse a log driver Ward does not have, and warn that options are
    /// ignored.
    ///
    /// # Errors
    ///
    /// Fails with the driver, why it is refused and where to read more.
    pub fn check(&self) -> Result<()> {
        if let Some(driver) = &self.log_driver
            && !ACCEPTED_DRIVERS.contains(&driver.as_str())
        {
            bail!(
                "--log-driver {driver} is not supported: Ward keeps sandbox output itself and only accepts json-file or local. See {DOCS}"
            );
        }
        if let Some(warning) = self.ignored_opts_warning() {
            eprintln!("{warning}");
        }
        Ok(())
    }

    /// What to tell the user when `--log-opt` was given.
    fn ignored_opts_warning(&self) -> Option<&'static str> {
        (!self.log_opt.is_empty()).then_some(IGNORED_OPTS_WARNING)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(name: &str) -> LogFlags {
        LogFlags {
            log_driver: Some(name.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn given_no_flags_when_checked_then_it_passes() {
        assert!(LogFlags::default().check().is_ok());
    }

    #[rstest::rstest]
    #[case::json_file("json-file")]
    #[case::local("local")]
    fn given_an_accepted_driver_when_checked_then_it_passes(#[case] name: &str) {
        assert!(driver(name).check().is_ok());
    }

    #[rstest::rstest]
    #[case::none("none")]
    #[case::syslog("syslog")]
    #[case::journald("journald")]
    fn given_another_driver_when_checked_then_the_error_names_it_in_one_line(#[case] name: &str) {
        let error = driver(name).check().unwrap_err().to_string();

        assert!(error.starts_with(&format!("--log-driver {name} is not supported")));
        assert!(error.ends_with(DOCS), "{error}");
        assert_eq!(error.lines().count(), 1);
    }

    #[test]
    fn given_log_opts_when_checked_then_a_warning_is_due_and_without_them_none() {
        let with = LogFlags {
            log_opt: vec!["max-size=10m".into()],
            ..Default::default()
        };

        assert_eq!(with.ignored_opts_warning(), Some(IGNORED_OPTS_WARNING));
        assert_eq!(LogFlags::default().ignored_opts_warning(), None);
    }

    #[test]
    fn given_log_opts_with_an_accepted_driver_when_checked_then_it_passes() {
        let flags = LogFlags {
            log_driver: Some("json-file".into()),
            log_opt: vec!["max-size=10m".into()],
        };

        assert!(flags.check().is_ok());
    }
}
