// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward update`: changes the limits and restart policy of sandboxes, which
//! may be running. Like the other lifecycle commands it prints each
//! reference on success and reports each failure on stderr.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;
use ward_core::pb::{RestartPolicy as PbRestartPolicy, UpdateSandboxRequest};
use ward_core::protocol::{NANOS_PER_CPU, RestartPolicy};

use crate::client;
use crate::lifecycle::Failures;
use crate::resources;

/// Docker's wording when no flag is given.
const NO_FLAGS: &str = "you must provide one or more flags when using this command";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Sandbox names, IDs or ID prefixes.
    #[arg(required = true)]
    pub refs: Vec<String>,
    /// Memory limit: bytes, or a number with a k, m, g or t suffix. 0 removes
    /// it. A running sandbox cannot go above the memory its VM started with.
    #[arg(short = 'm', long, value_name = "SIZE")]
    pub memory: Option<String>,
    /// CPU limit, fractions allowed. 0 removes it. A running sandbox cannot
    /// go above the CPUs its VM started with.
    #[arg(long, value_name = "NUMBER")]
    pub cpus: Option<String>,
    /// Process limit: 0 or -1 for unlimited.
    #[arg(long = "pids-limit", allow_negative_numbers = true, value_name = "N")]
    pub pids_limit: Option<i64>,
    /// What to do when the main process ends: no, on-failure[:N], always or
    /// unless-stopped.
    #[arg(long, value_name = "POLICY", value_parser = RestartPolicy::parse)]
    pub restart: Option<RestartPolicy>,
}

impl Args {
    /// The request for one sandbox, or an error for a bad value or no flag.
    ///
    /// # Errors
    ///
    /// Fails when no flag is given or a value cannot be parsed.
    pub fn request(&self, id: &str) -> Result<UpdateSandboxRequest> {
        let request = UpdateSandboxRequest {
            id: id.to_string(),
            memory_mb: self
                .memory
                .as_deref()
                .map(resources::parse_memory_mib)
                .transpose()?,
            nano_cpus: self.cpus.as_deref().map(parse_nano_cpus).transpose()?,
            pids_max: self
                .pids_limit
                .map(resources::parse_pids_limit)
                .transpose()?,
            restart_policy: self.restart.map(|policy| PbRestartPolicy {
                name: policy.kind.as_str().to_string(),
                maximum_retry_count: policy.max_retries,
            }),
        };
        if request.memory_mb.is_none()
            && request.nano_cpus.is_none()
            && request.pids_max.is_none()
            && request.restart_policy.is_none()
        {
            bail!(NO_FLAGS);
        }
        Ok(request)
    }
}

/// Parses `--cpus` into billionths of a CPU.
fn parse_nano_cpus(value: &str) -> Result<u64> {
    let cpus = value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|cpus| cpus.is_finite() && *cpus >= 0.0)
        .ok_or_else(|| {
            anyhow::anyhow!("invalid CPU count '{value}': use a number such as 2 or 0.5")
        })?;
    let nanos = (cpus * NANOS_PER_CPU as f64).round();
    if nanos > u64::MAX as f64 {
        bail!("CPU count '{value}' is too large");
    }
    Ok(nanos as u64)
}

pub async fn run(socket: &str, token_store: &Path, args: Args, json: bool) -> Result<()> {
    // Checked before connecting, so a mistake costs no round trip.
    args.request("")?;
    let mut c = client::connect(socket).await?;
    let mut failures = Failures::new();
    for reference in &args.refs {
        let result = async {
            let target = client::resolve(&mut c, token_store, reference).await?;
            c.update_sandbox(target.request(args.request(&target.id)?)?)
                .await?;
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) if json => println!("{}", json!({"updated": reference})),
            Ok(()) => println!("{reference}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(args.refs.len(), "updated")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Args {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: Args,
        }
        Wrapper::try_parse_from(["update", "web1"].iter().chain(flags))
            .expect("parse")
            .args
    }

    #[test]
    fn given_all_flags_when_request_built_then_each_value_is_converted() {
        // Arrange
        let parsed = args(&[
            "--memory",
            "512m",
            "--cpus",
            "1.5",
            "--pids-limit",
            "-1",
            "--restart",
            "on-failure:2",
        ]);

        // Act
        let request = parsed.request("id1").unwrap();

        // Assert
        assert_eq!(request.memory_mb, Some(512));
        assert_eq!(request.nano_cpus, Some(1_500_000_000));
        assert_eq!(request.pids_max, Some(0));
        let policy = request.restart_policy.expect("policy");
        assert_eq!(
            (policy.name.as_str(), policy.maximum_retry_count),
            ("on-failure", 2)
        );
    }

    #[test]
    fn given_only_one_flag_when_request_built_then_the_others_stay_unset() {
        // Arrange
        let parsed = args(&["--pids-limit", "64"]);

        // Act
        let request = parsed.request("id1").unwrap();

        // Assert
        assert_eq!(request.pids_max, Some(64));
        assert_eq!(request.memory_mb, None);
        assert_eq!(request.nano_cpus, None);
        assert!(request.restart_policy.is_none());
    }

    #[test]
    fn given_no_flags_when_request_built_then_it_fails_like_docker() {
        // Arrange
        let parsed = args(&[]);

        // Act
        let err = parsed.request("id1").unwrap_err();

        // Assert
        assert!(err.to_string().contains("one or more flags"), "{err}");
    }

    #[rstest::rstest]
    #[case::whole("2", 2_000_000_000)]
    #[case::fraction("0.5", 500_000_000)]
    #[case::unlimited("0", 0)]
    fn given_cpus_when_parsed_then_nanos(#[case] text: &str, #[case] want: u64) {
        assert_eq!(parse_nano_cpus(text).unwrap(), want);
    }

    #[rstest::rstest]
    #[case::negative("-1")]
    #[case::words("many")]
    #[case::not_finite("inf")]
    fn given_bad_cpus_when_parsed_then_it_fails(#[case] text: &str) {
        assert!(parse_nano_cpus(text).is_err());
    }
}
