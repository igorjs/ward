// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward egress suggest SANDBOX`: turn blocked destinations into
//! `--allow-domain` flags.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;
use ward_core::pb::{BlockedDestination, BlockedEgressRequest};

use crate::client;

/// A running sandbox's allowlist is fixed when it is created, so `--apply`
/// has nothing to update.
const APPLY_UNSUPPORTED: &str = "--apply is not supported: a sandbox's egress allowlist is fixed \
     when it is created. Recreate the sandbox with the suggested --allow-domain flags";

/// Host names the guest asked for are untrusted, and the output is meant to
/// be pasted into a shell, so only plain DNS name characters pass.
fn is_safe_name(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Splits blocked hosts into `--allow-domain` flags (one per name) and the
/// IP addresses an allowlist cannot express.
fn flags_and_ips(destinations: &[BlockedDestination]) -> (Vec<String>, Vec<String>) {
    let mut flags = Vec::new();
    let mut ips = Vec::new();
    for d in destinations {
        if d.host.parse::<IpAddr>().is_ok() {
            ips.push(d.host.clone());
        } else if is_safe_name(&d.host) {
            flags.push(format!("--allow-domain {}", d.host));
        }
    }
    (flags, ips)
}

/// Prints the flags that would allow what `reference` was refused.
pub async fn suggest(
    socket: &str,
    token_store: &Path,
    reference: &str,
    apply: bool,
    json: bool,
) -> Result<()> {
    if apply {
        bail!(APPLY_UNSUPPORTED);
    }
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let request = target.request(BlockedEgressRequest {
        sandbox_id: target.id.clone(),
    })?;
    let reply = c.blocked_egress(request).await?.into_inner();
    let (flags, ips) = flags_and_ips(&reply.destinations);
    if json {
        println!("{}", json!({"AllowDomainFlags": flags, "BlockedIPs": ips}));
        return Ok(());
    }
    for flag in &flags {
        println!("{flag}");
    }
    if !ips.is_empty() {
        eprintln!(
            "Blocked by IP address, which an allowlist cannot match: {}",
            ips.join(", ")
        );
    }
    if flags.is_empty() && ips.is_empty() {
        eprintln!("No blocked destinations recorded for {reference}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(host: &str) -> BlockedDestination {
        BlockedDestination {
            host: host.into(),
            count: 1,
            last_seen_unix: 0,
        }
    }

    #[test]
    fn given_names_when_formatted_then_one_allow_flag_per_name() {
        // Arrange
        let blocked = [dest("pypi.org"), dest("files.pythonhosted.org")];

        // Act
        let (flags, ips) = flags_and_ips(&blocked);

        // Assert
        assert_eq!(
            flags,
            [
                "--allow-domain pypi.org",
                "--allow-domain files.pythonhosted.org"
            ]
        );
        assert!(ips.is_empty());
    }

    #[test]
    fn given_ip_host_when_formatted_then_listed_apart_from_flags() {
        // Arrange
        let blocked = [dest("203.0.113.9"), dest("2001:db8::1")];

        // Act
        let (flags, ips) = flags_and_ips(&blocked);

        // Assert
        assert!(flags.is_empty());
        assert_eq!(ips, ["203.0.113.9", "2001:db8::1"]);
    }

    #[test]
    fn given_shell_metacharacters_when_formatted_then_name_is_dropped() {
        // Arrange
        let blocked = [dest("a.com;rm -rf ~"), dest("$(id).evil.com")];

        // Act
        let (flags, ips) = flags_and_ips(&blocked);

        // Assert
        assert!(flags.is_empty());
        assert!(ips.is_empty());
    }

    #[tokio::test]
    async fn given_apply_when_suggest_then_fails_before_connecting() {
        // Arrange / Act
        let err = suggest("/nonexistent.sock", Path::new("/x"), "sb", true, false)
            .await
            .expect_err("apply must fail");

        // Assert
        assert!(err.to_string().contains("--apply is not supported"));
    }
}
