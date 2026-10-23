// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `-p/--publish` parsing and `ward port`.
//!
//! The syntax is Docker's, `[HOST_IP:][HOST_PORT:]CONTAINER_PORT[/tcp]`,
//! except that a missing HOST_IP means 127.0.0.1 and not 0.0.0.0, so a guest
//! is only reachable from the network when the user asks for it.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;
use ward_core::pb::{GetSandboxRequest, PortMapping};

use crate::client;

const PROTOCOL: &str = "tcp";

/// Parse one `-p` value.
pub fn parse_publish(spec: &str) -> Result<PortMapping> {
    let (body, protocol) = split_protocol(spec)?;
    let (host_ip, ports) = split_host_ip(body, spec)?;
    let (host_port, container_port) = match ports.split(':').collect::<Vec<_>>().as_slice() {
        [container] => (0, parse_port(container, spec)?),
        [host, container] => (parse_host_port(host, spec)?, parse_port(container, spec)?),
        _ => {
            bail!("invalid publish value \"{spec}\": expected [HOST_IP:][HOST_PORT:]CONTAINER_PORT")
        }
    };
    if container_port == 0 {
        bail!("invalid publish value \"{spec}\": the container port must be between 1 and 65535");
    }
    Ok(PortMapping {
        host_ip,
        host_port: u32::from(host_port),
        container_port: u32::from(container_port),
        protocol: protocol.to_string(),
    })
}

pub fn parse_all(specs: &[String]) -> Result<Vec<PortMapping>> {
    specs.iter().map(|spec| parse_publish(spec)).collect()
}

/// Tell the user when a mapping opens the guest to the network.
pub fn warn_if_exposed(mappings: &[PortMapping]) {
    for mapping in mappings {
        if binds_all_interfaces(mapping) {
            eprintln!(
                "WARNING: port {} is published on all host interfaces and is reachable from the network",
                mapping.container_port
            );
        }
    }
}

fn binds_all_interfaces(mapping: &PortMapping) -> bool {
    mapping
        .host_ip
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified())
}

/// `80/tcp` or `80`; returns the port and the protocol.
fn split_protocol(spec: &str) -> Result<(&str, &str)> {
    let Some((body, protocol)) = spec.rsplit_once('/') else {
        return Ok((spec, PROTOCOL));
    };
    if protocol != PROTOCOL {
        bail!("unsupported protocol \"{protocol}\" in \"{spec}\": only tcp ports can be published");
    }
    Ok((body, protocol))
}

/// Split a leading `IP:` or `[IPV6]:` off `body`. An address is present when
/// there are three fields, or the first one is bracketed.
fn split_host_ip<'a>(body: &'a str, spec: &str) -> Result<(String, &'a str)> {
    if let Some(rest) = body.strip_prefix('[') {
        let (ip, ports) = rest
            .split_once("]:")
            .with_context(|| format!("invalid publish value \"{spec}\": unclosed '['"))?;
        return Ok((ip.to_string(), ports));
    }
    match body.splitn(2, ':').collect::<Vec<_>>().as_slice() {
        [ip, ports] if ports.contains(':') => Ok(((*ip).to_string(), ports)),
        _ => Ok((String::new(), body)),
    }
}

fn parse_host_port(text: &str, spec: &str) -> Result<u16> {
    if text.is_empty() {
        return Ok(0);
    }
    parse_port(text, spec)
}

fn parse_port(text: &str, spec: &str) -> Result<u16> {
    if text.contains('-') {
        bail!("invalid publish value \"{spec}\": port ranges are not supported");
    }
    text.parse()
        .with_context(|| format!("invalid publish value \"{spec}\": \"{text}\" is not a port"))
}

/// `ward port SANDBOX [PORT[/tcp]]`: print where each published port is
/// reachable on the host.
pub async fn run(
    socket: &str,
    token_store: &Path,
    reference: &str,
    port: Option<&str>,
    json: bool,
) -> Result<()> {
    let wanted = port.map(parse_private_port).transpose()?;
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let request = target.request(GetSandboxRequest {
        id: target.id.clone(),
    })?;
    let info = c.get_sandbox(request).await?.into_inner();
    let shown: Vec<&PortMapping> = info
        .published_ports
        .iter()
        .filter(|m| wanted.is_none_or(|port| m.container_port == u32::from(port)))
        .collect();
    if let Some(port) = wanted
        && shown.is_empty()
    {
        bail!("No public port '{port}/{PROTOCOL}' published for {reference}");
    }
    if json {
        let rows: Vec<_> = shown.iter().map(|m| mapping_json(m)).collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        for mapping in shown {
            println!("{}", mapping_line(mapping, wanted.is_some()));
        }
    }
    Ok(())
}

fn parse_private_port(text: &str) -> Result<u16> {
    let (body, _) = split_protocol(text)?;
    parse_port(body, text)
}

/// `80/tcp -> 127.0.0.1:49153`, or just the address when a port was asked for.
fn mapping_line(mapping: &PortMapping, address_only: bool) -> String {
    let address = host_address(mapping);
    if address_only {
        address
    } else {
        format!("{}/{PROTOCOL} -> {address}", mapping.container_port)
    }
}

fn host_address(mapping: &PortMapping) -> String {
    if mapping.host_ip.contains(':') {
        format!("[{}]:{}", mapping.host_ip, mapping.host_port)
    } else {
        format!("{}:{}", mapping.host_ip, mapping.host_port)
    }
}

fn mapping_json(mapping: &PortMapping) -> serde_json::Value {
    json!({
        "container_port": mapping.container_port,
        "protocol": PROTOCOL,
        "host_ip": mapping.host_ip,
        "host_port": mapping.host_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::container_only("80", "", 0, 80)]
    #[case::host_and_container("8080:80", "", 8080, 80)]
    #[case::any_host_port("0:80", "", 0, 80)]
    #[case::loopback("127.0.0.1:8080:80", "127.0.0.1", 8080, 80)]
    #[case::all_interfaces("0.0.0.0:8080:80", "0.0.0.0", 8080, 80)]
    #[case::random_host_port_on_address("127.0.0.1::80", "127.0.0.1", 0, 80)]
    #[case::tcp_suffix("8080:80/tcp", "", 8080, 80)]
    #[case::ipv6("[::1]:8080:80", "::1", 8080, 80)]
    fn given_a_publish_value_when_parsed_then_the_mapping_matches(
        #[case] spec: &str,
        #[case] host_ip: &str,
        #[case] host_port: u32,
        #[case] container_port: u32,
    ) {
        // Act
        let mapping = parse_publish(spec).unwrap();

        // Assert
        assert_eq!(mapping.host_ip, host_ip);
        assert_eq!(mapping.host_port, host_port);
        assert_eq!(mapping.container_port, container_port);
        assert_eq!(mapping.protocol, PROTOCOL);
    }

    #[rstest]
    #[case::udp("8080:53/udp", "only tcp")]
    #[case::range("8000-8010:80", "ranges")]
    #[case::container_range("8080:80-90", "ranges")]
    #[case::zero_container("8080:0", "between 1 and 65535")]
    #[case::too_big("70000", "not a port")]
    #[case::text("web", "not a port")]
    #[case::too_many_fields("1.2.3.4:1:2:3", "expected")]
    #[case::unclosed_bracket("[::1:80", "unclosed")]
    fn given_an_unsupported_publish_value_when_parsed_then_it_is_rejected_with_a_reason(
        #[case] spec: &str,
        #[case] reason: &str,
    ) {
        // Act
        let err = parse_publish(spec).unwrap_err();

        // Assert
        assert!(err.to_string().contains(reason), "{err}");
    }

    fn mapping(host_ip: &str, host_port: u32, container_port: u32) -> PortMapping {
        PortMapping {
            host_ip: host_ip.into(),
            host_port,
            container_port,
            protocol: PROTOCOL.into(),
        }
    }

    #[rstest]
    #[case::v4("0.0.0.0:8080:80", true)]
    #[case::v6("[::]:8080:80", true)]
    #[case::loopback("127.0.0.1:8080:80", false)]
    #[case::v6_loopback("[::1]:8080:80", false)]
    fn given_a_publish_value_when_checked_then_only_an_unspecified_address_is_flagged(
        #[case] spec: &str,
        #[case] flagged: bool,
    ) {
        // Arrange
        let mapping = parse_publish(spec).unwrap();

        // Act
        let result = binds_all_interfaces(&mapping);

        // Assert
        assert_eq!(result, flagged);
    }

    #[test]
    fn given_a_mapping_when_listed_then_it_reads_like_docker_port() {
        // Act
        let line = mapping_line(&mapping("127.0.0.1", 49153, 80), false);

        // Assert
        assert_eq!(line, "80/tcp -> 127.0.0.1:49153");
    }

    #[test]
    fn given_a_private_port_when_listed_then_only_the_address_is_printed() {
        // Act
        let line = mapping_line(&mapping("::1", 49153, 80), true);

        // Assert
        assert_eq!(line, "[::1]:49153");
    }

    #[rstest]
    #[case::port("80", 80)]
    #[case::port_with_protocol("443/tcp", 443)]
    fn given_a_private_port_argument_when_parsed_then_the_port_is_returned(
        #[case] text: &str,
        #[case] expected: u16,
    ) {
        // Act
        let port = parse_private_port(text).unwrap();

        // Assert
        assert_eq!(port, expected);
    }

    #[test]
    fn given_a_udp_private_port_when_parsed_then_it_is_rejected() {
        // Act
        let err = parse_private_port("53/udp").unwrap_err();

        // Assert
        assert!(err.to_string().contains("only tcp"), "{err}");
    }
}
