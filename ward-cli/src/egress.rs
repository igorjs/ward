// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward egress suggest SANDBOX`: turn blocked destinations into
//! `--allow-domain` flags, or add them to the sandbox's allowlist.

use std::net::IpAddr;
use std::path::Path;

use anyhow::Result;
use serde_json::json;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{AddEgressAllowRequest, BlockedDestination, BlockedEgressRequest};

use crate::client;

/// Host names the guest asked for are untrusted, and the output is meant to
/// be pasted into a shell, so only plain DNS name characters pass.
fn is_safe_name(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Splits blocked hosts into the DNS names an allowlist can hold and the IP
/// addresses it cannot express.
fn names_and_ips(destinations: &[BlockedDestination]) -> (Vec<String>, Vec<String>) {
    let mut names = Vec::new();
    let mut ips = Vec::new();
    for d in destinations {
        if d.host.parse::<IpAddr>().is_ok() {
            ips.push(d.host.clone());
        } else if is_safe_name(&d.host) {
            names.push(d.host.clone());
        }
    }
    (names, ips)
}

fn allow_flags(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| format!("--allow-domain {name}"))
        .collect()
}

fn warn_about_ips(ips: &[String]) {
    if !ips.is_empty() {
        eprintln!(
            "Blocked by IP address, which an allowlist cannot match: {}",
            ips.join(", ")
        );
    }
}

/// Prints the flags that would allow what `reference` was refused, or with
/// `apply` adds those hosts to its allowlist.
pub async fn suggest(
    socket: &str,
    token_store: &Path,
    reference: &str,
    apply: bool,
    json: bool,
) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let request = target.request(BlockedEgressRequest {
        sandbox_id: target.id.clone(),
    })?;
    let reply = c.blocked_egress(request).await?.into_inner();
    let (names, ips) = names_and_ips(&reply.destinations);
    if apply {
        return apply_names(&mut c, &target, reference, names, &ips, json).await;
    }
    let flags = allow_flags(&names);
    if json {
        println!("{}", json!({"AllowDomainFlags": flags, "BlockedIPs": ips}));
        return Ok(());
    }
    for flag in &flags {
        println!("{flag}");
    }
    warn_about_ips(&ips);
    if flags.is_empty() && ips.is_empty() {
        eprintln!("No blocked destinations recorded for {reference}");
    }
    Ok(())
}

/// Adds `names` to the sandbox's allowlist and prints what the daemon added.
async fn apply_names(
    c: &mut WardClient<Channel>,
    target: &client::Resolved,
    reference: &str,
    names: Vec<String>,
    ips: &[String],
    json: bool,
) -> Result<()> {
    let added = if names.is_empty() {
        Vec::new()
    } else {
        let request = target.request(AddEgressAllowRequest {
            sandbox_id: target.id.clone(),
            domains: names,
        })?;
        c.add_egress_allow(request).await?.into_inner().added
    };
    if json {
        println!("{}", json!({"Added": added, "BlockedIPs": ips}));
        return Ok(());
    }
    for name in &added {
        println!("Added {name} to the egress allowlist of {reference}");
    }
    warn_about_ips(ips);
    if added.is_empty() {
        eprintln!("Nothing to add to the egress allowlist of {reference}");
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
        let (names, ips) = names_and_ips(&blocked);

        // Assert
        assert_eq!(
            allow_flags(&names),
            [
                "--allow-domain pypi.org",
                "--allow-domain files.pythonhosted.org"
            ]
        );
        assert!(ips.is_empty());
    }

    #[test]
    fn given_ip_host_when_split_then_listed_apart_from_names() {
        // Arrange
        let blocked = [dest("203.0.113.9"), dest("2001:db8::1"), dest("pypi.org")];

        // Act
        let (names, ips) = names_and_ips(&blocked);

        // Assert
        assert_eq!(names, ["pypi.org"]);
        assert_eq!(ips, ["203.0.113.9", "2001:db8::1"]);
    }

    #[test]
    fn given_shell_metacharacters_when_split_then_name_is_dropped() {
        // Arrange
        let blocked = [dest("a.com;rm -rf ~"), dest("$(id).evil.com")];

        // Act
        let (names, ips) = names_and_ips(&blocked);

        // Assert
        assert!(names.is_empty());
        assert!(ips.is_empty());
    }
}
