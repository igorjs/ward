// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Extra `/etc/hosts` lines and the search domains and options of
//! `/etc/resolv.conf`.

/// Argument that adds a hosts line: `--add-host <name> <ip>`.
pub const ADD_HOST_ARG: &str = "--add-host";
/// Argument that adds a search domain: `--dns-search <domain>`.
pub const DNS_SEARCH_ARG: &str = "--dns-search";
/// Argument that adds a resolver option: `--dns-option <option>`.
pub const DNS_OPTION_ARG: &str = "--dns-option";

#[cfg(target_os = "linux")]
const HOSTS_PATH: &str = "/etc/hosts";
#[cfg(target_os = "linux")]
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";
const SEARCH_KEYWORD: &str = "search";
const OPTIONS_KEYWORD: &str = "options";
/// `domain` and `search` override each other, so ours replace both.
const DOMAIN_KEYWORD: &str = "domain";

/// The guest's host names and addresses the daemon asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestHost {
    pub name: String,
    pub address: String,
}

/// `existing` hosts text with one line added per host.
#[must_use]
pub fn hosts_with(existing: &str, hosts: &[GuestHost]) -> String {
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for host in hosts {
        text.push_str(&format!("{}\t{}\n", host.address, host.name));
    }
    text
}

/// `existing` resolver config with its `search`, `domain` and `options` lines
/// replaced by ours. Lines are left alone when we have nothing for them.
#[must_use]
pub fn resolv_conf_with(existing: &str, search: &[String], options: &[String]) -> String {
    let drop_search = !search.is_empty();
    let drop_options = !options.is_empty();
    let mut text: String = existing
        .lines()
        .filter(|line| {
            let keyword = line.split_whitespace().next().unwrap_or_default();
            let replaced = match keyword {
                OPTIONS_KEYWORD => drop_options,
                SEARCH_KEYWORD | DOMAIN_KEYWORD => drop_search,
                _ => false,
            };
            !replaced
        })
        .map(|line| format!("{line}\n"))
        .collect();
    if drop_search {
        text.push_str(&format!("{SEARCH_KEYWORD} {}\n", search.join(" ")));
    }
    if drop_options {
        text.push_str(&format!("{OPTIONS_KEYWORD} {}\n", options.join(" ")));
    }
    text
}

/// Write the hosts lines and resolver settings. Call it after the network is
/// configured, since that writes the resolver's name server.
///
/// # Errors
///
/// Fails when a file cannot be read or written.
#[cfg(target_os = "linux")]
pub fn apply(hosts: &[GuestHost], search: &[String], options: &[String]) -> std::io::Result<()> {
    use std::fs;

    let read = |path| fs::read_to_string(path).or_else(not_found_is_empty);
    if !hosts.is_empty() {
        fs::write(HOSTS_PATH, hosts_with(&read(HOSTS_PATH)?, hosts))?;
    }
    if !search.is_empty() || !options.is_empty() {
        let text = resolv_conf_with(&read(RESOLV_CONF_PATH)?, search, options);
        fs::write(RESOLV_CONF_PATH, text)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn not_found_is_empty(error: std::io::Error) -> std::io::Result<String> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(String::new())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, address: &str) -> GuestHost {
        GuestHost {
            name: name.into(),
            address: address.into(),
        }
    }

    #[test]
    fn hosts_are_appended_after_a_missing_final_newline() {
        // Act
        let text = hosts_with("127.0.0.1 localhost", &[host("db", "10.1.2.3")]);

        // Assert
        assert_eq!(text, "127.0.0.1 localhost\n10.1.2.3\tdb\n");
    }

    #[test]
    fn resolv_conf_replaces_search_and_options_but_keeps_name_servers() {
        // Arrange
        let existing = "nameserver 10.0.2.2\nsearch old.example\noptions ndots:1\n";

        // Act
        let text = resolv_conf_with(
            existing,
            &["a.example".into(), "b.example".into()],
            &["ndots:3".into()],
        );

        // Assert
        assert_eq!(
            text,
            "nameserver 10.0.2.2\nsearch a.example b.example\noptions ndots:3\n"
        );
    }

    #[test]
    fn resolv_conf_keeps_lines_it_has_nothing_to_replace() {
        // Act
        let text = resolv_conf_with(
            "search old.example\noptions rotate\n",
            &[],
            &["ndots:2".into()],
        );

        // Assert
        assert_eq!(text, "search old.example\noptions ndots:2\n");
    }
}
