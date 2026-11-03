// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `--add-host`, `--dns-search` and `--dns-option`: validation of the lists a
//! request carries and the guest agent arguments that apply them.

use std::net::IpAddr;

use crate::protocol::ApiError;

/// Docker's value for the address of the host as the sandbox reaches it.
pub const HOST_GATEWAY: &str = "host-gateway";
/// The host stack's address inside the guest, which `host-gateway` stands for.
pub const GATEWAY_ADDRESS: &str = "10.0.2.2";
const HOST_IP_SEPARATOR: char = ':';
/// Most `/etc/hosts` lines one sandbox adds.
pub const MAX_EXTRA_HOSTS: usize = 256;
/// Longest host name, as DNS allows.
const MAX_HOST_NAME_LEN: usize = 253;
/// glibc reads at most six search domains of 256 characters in all.
pub const MAX_SEARCH_DOMAINS: usize = 6;
const MAX_SEARCH_CHARS: usize = 256;
/// Most resolver options one sandbox adds.
pub const MAX_DNS_OPTIONS: usize = 32;
const MAX_OPTION_LEN: usize = 64;

/// Agent argument that adds a hosts line: `--add-host <name> <ip>`.
pub const ADD_HOST_ARG: &str = "--add-host";
/// Agent argument that adds a search domain: `--dns-search <domain>`.
pub const DNS_SEARCH_ARG: &str = "--dns-search";
/// Agent argument that adds a resolver option: `--dns-option <option>`.
pub const DNS_OPTION_ARG: &str = "--dns-option";

fn invalid(message: String) -> ApiError {
    ApiError::InvalidRequest(message)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')
}

/// Check `HOST:IP` entries and return them with `host-gateway` resolved and
/// IPv6 brackets removed.
///
/// # Errors
///
/// Fails on a missing or bad host name or address, or too many entries.
pub fn extra_hosts(entries: &[String]) -> Result<Vec<String>, ApiError> {
    if entries.len() > MAX_EXTRA_HOSTS {
        return Err(invalid(format!(
            "at most {MAX_EXTRA_HOSTS} --add-host entries are allowed"
        )));
    }
    entries.iter().map(|entry| extra_host(entry)).collect()
}

fn extra_host(entry: &str) -> Result<String, ApiError> {
    let bad = |why: &str| invalid(format!("invalid --add-host '{entry}': {why}"));
    let (name, address) = entry
        .split_once(HOST_IP_SEPARATOR)
        .ok_or_else(|| bad("use HOST:IP"))?;
    if name.is_empty() || name.len() > MAX_HOST_NAME_LEN || !name.chars().all(is_name_char) {
        return Err(bad(
            "the host name is empty, too long or has a bad character",
        ));
    }
    let address = address.trim_start_matches('[').trim_end_matches(']');
    if address == HOST_GATEWAY {
        return Ok(format!("{name}{HOST_IP_SEPARATOR}{GATEWAY_ADDRESS}"));
    }
    let ip: IpAddr = address
        .parse()
        .map_err(|_| bad("the address is not an IP"))?;
    Ok(format!("{name}{HOST_IP_SEPARATOR}{ip}"))
}

/// Check search domains.
///
/// # Errors
///
/// Fails on a bad domain, or more than glibc reads.
pub fn search_domains(domains: &[String]) -> Result<(), ApiError> {
    let chars: usize = domains.iter().map(|d| d.len() + 1).sum();
    if domains.len() > MAX_SEARCH_DOMAINS || chars > MAX_SEARCH_CHARS {
        return Err(invalid(format!(
            "--dns-search takes at most {MAX_SEARCH_DOMAINS} domains and {MAX_SEARCH_CHARS} characters"
        )));
    }
    for domain in domains {
        if domain.is_empty()
            || domain.len() > MAX_HOST_NAME_LEN
            || !domain.chars().all(is_name_char)
        {
            return Err(invalid(format!("invalid --dns-search '{domain}'")));
        }
    }
    Ok(())
}

/// Check resolver options such as `ndots:2` or `rotate`.
///
/// # Errors
///
/// Fails on a bad option or too many.
pub fn dns_options(options: &[String]) -> Result<(), ApiError> {
    if options.len() > MAX_DNS_OPTIONS {
        return Err(invalid(format!(
            "at most {MAX_DNS_OPTIONS} --dns-option entries are allowed"
        )));
    }
    for option in options {
        let ok = !option.is_empty()
            && option.len() <= MAX_OPTION_LEN
            && option
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'));
        if !ok {
            return Err(invalid(format!("invalid --dns-option '{option}'")));
        }
    }
    Ok(())
}

/// The agent arguments that apply the lists at boot.
#[must_use]
pub fn agent_args(hosts: &[String], search: &[String], options: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    for entry in hosts {
        if let Some((name, address)) = entry.split_once(HOST_IP_SEPARATOR) {
            args.extend([
                ADD_HOST_ARG.to_string(),
                name.to_string(),
                address.to_string(),
            ]);
        }
    }
    for domain in search {
        args.extend([DNS_SEARCH_ARG.to_string(), domain.clone()]);
    }
    for option in options {
        args.extend([DNS_OPTION_ARG.to_string(), option.clone()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn extra_hosts_normalise_brackets_and_resolve_host_gateway() {
        // Act
        let hosts = extra_hosts(&strings(&["db:10.1.2.3", "v6:[::1]", "host:host-gateway"]));

        // Assert
        assert_eq!(hosts.unwrap(), ["db:10.1.2.3", "v6:::1", "host:10.0.2.2"]);
    }

    #[rstest::rstest]
    #[case::no_separator("db")]
    #[case::no_name(":10.0.0.1")]
    #[case::bad_ip("db:not-an-ip")]
    #[case::bad_name("my host:10.0.0.1")]
    fn extra_hosts_refuse_bad_entries(#[case] entry: &str) {
        assert!(extra_hosts(&strings(&[entry])).is_err());
    }

    #[test]
    fn search_domains_are_capped_like_glibc() {
        // Arrange
        let many = strings(&["a", "b", "c", "d", "e", "f", "g"]);

        // Act / Assert
        assert!(search_domains(&many).is_err());
        assert!(search_domains(&strings(&["example.com", "corp.local"])).is_ok());
        assert!(search_domains(&strings(&["bad domain"])).is_err());
    }

    #[test]
    fn dns_options_accept_resolver_words_only() {
        assert!(dns_options(&strings(&["ndots:2", "rotate", "timeout:3"])).is_ok());
        assert!(dns_options(&strings(&["ndots 2"])).is_err());
    }

    #[test]
    fn agent_args_split_each_host_and_list_domains_then_options() {
        // Act
        let args = agent_args(
            &strings(&["db:10.1.2.3"]),
            &strings(&["example.com"]),
            &strings(&["ndots:2"]),
        );

        // Assert
        assert_eq!(
            args,
            [
                "--add-host",
                "db",
                "10.1.2.3",
                "--dns-search",
                "example.com",
                "--dns-option",
                "ndots:2"
            ]
        );
    }
}
