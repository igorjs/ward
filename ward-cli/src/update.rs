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

/// The `--memory-swap` text for unlimited swap.
const UNLIMITED_SWAP: &str = "-1";
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
    /// Only a value equal to --memory is accepted: the guest has no swap.
    #[arg(long = "memory-swap", value_name = "SIZE", allow_hyphen_values = true)]
    pub memory_swap: Option<String>,
    /// Memory the guest cgroup protects from reclaim (memory.low). Must not
    /// exceed --memory.
    #[arg(long = "memory-reservation", value_name = "SIZE")]
    pub memory_reservation: Option<String>,
    /// Relative CPU weight, 2 to 262144 (default 1024), as cgroup v2 cpu.weight.
    #[arg(short = 'c', long = "cpu-shares", value_name = "N")]
    pub cpu_shares: Option<u32>,
    /// CPUs the processes may use, such as 0-1,3 (cpuset.cpus).
    #[arg(long = "cpuset-cpus", value_name = "LIST")]
    pub cpuset_cpus: Option<String>,
    /// Length of a CPU scheduling period in microseconds, 1000 to 1000000.
    #[arg(long = "cpu-period", value_name = "MICROSECONDS")]
    pub cpu_period: Option<u64>,
    /// CPU time per period in microseconds, at least 1000; -1 or 0 for none.
    #[arg(
        long = "cpu-quota",
        value_name = "MICROSECONDS",
        allow_negative_numbers = true
    )]
    pub cpu_quota: Option<i64>,
    /// Docker flags with no meaning in a microVM. They are accepted so the
    /// command line parses, then refused with the reason.
    #[arg(long = "blkio-weight", value_name = "N", hide = true)]
    blkio_weight: Option<String>,
    #[arg(long = "cpuset-mems", value_name = "LIST", hide = true)]
    cpuset_mems: Option<String>,
    #[arg(long = "cpu-rt-period", value_name = "N", hide = true)]
    cpu_rt_period: Option<String>,
    #[arg(long = "cpu-rt-runtime", value_name = "N", hide = true)]
    cpu_rt_runtime: Option<String>,
}

/// Flags Docker has that the guest cannot honour, and why.
const REFUSED: [(&str, &str); 4] = [
    ("--blkio-weight", "the guest has no block I/O controller"),
    ("--cpuset-mems", "the guest has a single memory node"),
    ("--cpu-rt-period", "the guest runs no real-time scheduling"),
    ("--cpu-rt-runtime", "the guest runs no real-time scheduling"),
];

impl Args {
    /// The request for one sandbox, or an error for a bad value or no flag.
    ///
    /// # Errors
    ///
    /// Fails when no flag is given or a value cannot be parsed.
    pub fn request(&self, id: &str) -> Result<UpdateSandboxRequest> {
        let given = [
            &self.blkio_weight,
            &self.cpuset_mems,
            &self.cpu_rt_period,
            &self.cpu_rt_runtime,
        ];
        for ((flag, reason), value) in REFUSED.iter().zip(given) {
            if value.is_some() {
                bail!("{flag} is not supported: {reason}");
            }
        }
        let request = UpdateSandboxRequest {
            memory_swap_bytes: self
                .memory_swap
                .as_deref()
                .map(parse_memory_swap)
                .transpose()?,
            memory_reservation_bytes: self
                .memory_reservation
                .as_deref()
                .map(resources::parse_size_bytes)
                .transpose()?,
            cpu_shares: self.cpu_shares,
            cpuset_cpus: self.cpuset_cpus.clone(),
            cpu_period: self.cpu_period,
            cpu_quota: self.cpu_quota,
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
            && !changes_cgroup(&request)
        {
            bail!(NO_FLAGS);
        }
        Ok(request)
    }
}

fn changes_cgroup(request: &UpdateSandboxRequest) -> bool {
    request.memory_swap_bytes.is_some()
        || request.memory_reservation_bytes.is_some()
        || request.cpu_shares.is_some()
        || request.cpuset_cpus.is_some()
        || request.cpu_period.is_some()
        || request.cpu_quota.is_some()
}

/// `--memory-swap`: -1 for unlimited, else a size.
fn parse_memory_swap(value: &str) -> Result<i64> {
    if value.trim() == UNLIMITED_SWAP {
        return Ok(-1);
    }
    Ok(i64::try_from(resources::parse_size_bytes(value)?)?)
}

/// Parses `--cpus` into billionths of a CPU.
fn parse_nano_cpus(value: &str) -> Result<u64> {
    let cpus = resources::parse_cpu_count(value)?;
    Ok((cpus * NANOS_PER_CPU as f64).round() as u64)
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

    #[test]
    fn given_cgroup_flags_when_request_built_then_each_is_carried() {
        let parsed = args(&[
            "--cpu-shares",
            "512",
            "--cpuset-cpus",
            "0-1",
            "--memory-swap",
            "-1",
            "--memory-reservation",
            "64m",
            "--cpu-period",
            "100000",
            "--cpu-quota",
            "-1",
        ]);

        let request = parsed.request("id1").unwrap();

        assert_eq!(request.cpu_shares, Some(512));
        assert_eq!(request.cpuset_cpus.as_deref(), Some("0-1"));
        assert_eq!(request.memory_swap_bytes, Some(-1));
        assert_eq!(request.memory_reservation_bytes, Some(64 << 20));
        assert_eq!(request.cpu_period, Some(100_000));
        assert_eq!(request.cpu_quota, Some(-1));
    }

    #[test]
    fn given_a_cgroup_flag_alone_when_request_built_then_it_counts_as_a_flag() {
        assert!(args(&["-c", "100"]).request("id1").is_ok());
    }

    #[rstest::rstest]
    #[case::blkio(&["--blkio-weight", "100"], "--blkio-weight is not supported: the guest has no block I/O")]
    #[case::mems(&["--cpuset-mems", "0"], "--cpuset-mems is not supported")]
    #[case::rt(&["--cpu-rt-runtime", "5"], "--cpu-rt-runtime is not supported")]
    fn given_a_docker_flag_with_no_guest_meaning_when_request_built_then_the_reason_is_given(
        #[case] flags: &[&str],
        #[case] said: &str,
    ) {
        let err = args(flags).request("id1").unwrap_err();

        assert!(err.to_string().contains(said), "{err}");
    }
}
