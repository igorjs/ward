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
const UDP: &str = "udp";
const RANGE_SEPARATOR: char = '-';
/// Most ports one sandbox can publish; the daemon enforces the same limit.
const MAX_PUBLISHED_PORTS: usize = 64;

/// An inclusive run of ports; one port is a run of length 1.
#[derive(Clone, Copy)]
struct PortRange {
    first: u16,
    last: u16,
}

impl PortRange {
    fn len(self) -> usize {
        usize::from(self.last - self.first) + 1
    }

    fn is_single(self) -> bool {
        self.first == self.last
    }
}

/// Parse one `-p` value into the mappings it stands for: a range of equal
/// length on both sides is one mapping per port, a host range with one
/// container port lets the daemon pick a free host port from the range.
pub fn parse_publish(spec: &str) -> Result<Vec<PortMapping>> {
    let (body, protocol) = split_protocol(spec)?;
    let (host_ip, ports) = split_host_ip(body, spec)?;
    let (host, container) = match ports.split(':').collect::<Vec<_>>().as_slice() {
        [container] => (None, parse_range(container, spec)?),
        [host, container] => (parse_host_range(host, spec)?, parse_range(container, spec)?),
        _ => {
            bail!("invalid publish value \"{spec}\": expected [HOST_IP:][HOST_PORT:]CONTAINER_PORT")
        }
    };
    if container.len() > MAX_PUBLISHED_PORTS {
        bail!(
            "invalid publish value \"{spec}\": a range holds {} ports and at most {MAX_PUBLISHED_PORTS} can be published",
            container.len()
        );
    }
    let mapping = |host_port: u16, host_port_end: u16, container_port: u16| PortMapping {
        host_ip: host_ip.clone(),
        host_port: u32::from(host_port),
        container_port: u32::from(container_port),
        protocol: protocol.to_string(),
        host_port_end: u32::from(host_port_end),
    };
    let container_ports = container.first..=container.last;
    match host {
        None => Ok(container_ports.map(|port| mapping(0, 0, port)).collect()),
        Some(host) if host.is_single() => {
            if !container.is_single() {
                bail!(
                    "invalid publish value \"{spec}\": a container port range needs a host range of the same length"
                );
            }
            Ok(vec![mapping(host.first, 0, container.first)])
        }
        Some(host) if container.is_single() => {
            Ok(vec![mapping(host.first, host.last, container.first)])
        }
        Some(host) if host.len() == container.len() => Ok((host.first..=host.last)
            .zip(container_ports)
            .map(|(host_port, port)| mapping(host_port, 0, port))
            .collect()),
        Some(_) => bail!(
            "invalid publish value \"{spec}\": the host and container port ranges differ in length"
        ),
    }
}

pub fn parse_all(specs: &[String]) -> Result<Vec<PortMapping>> {
    let mut all = Vec::new();
    for spec in specs {
        all.extend(parse_publish(spec)?);
    }
    Ok(all)
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

/// `80/tcp`, `53/udp` or `80`; returns the ports and the protocol.
fn split_protocol(spec: &str) -> Result<(&str, &str)> {
    let Some((body, protocol)) = spec.rsplit_once('/') else {
        return Ok((spec, PROTOCOL));
    };
    if protocol != PROTOCOL && protocol != UDP {
        bail!("unsupported protocol \"{protocol}\" in \"{spec}\": use tcp or udp");
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

/// The host side of a mapping: nothing, `0` or empty mean any free port.
fn parse_host_range(text: &str, spec: &str) -> Result<Option<PortRange>> {
    if text.is_empty() || text == "0" {
        return Ok(None);
    }
    parse_range(text, spec).map(Some)
}

fn parse_range(text: &str, spec: &str) -> Result<PortRange> {
    let Some((first, last)) = text.split_once(RANGE_SEPARATOR) else {
        let port = parse_port(text, spec)?;
        return Ok(PortRange {
            first: port,
            last: port,
        });
    };
    let (first, last) = (parse_port(first, spec)?, parse_port(last, spec)?);
    if last < first {
        bail!("invalid publish value \"{spec}\": the port range {text} runs backwards");
    }
    Ok(PortRange { first, last })
}

fn parse_port(text: &str, spec: &str) -> Result<u16> {
    let port: u16 = text
        .parse()
        .with_context(|| format!("invalid publish value \"{spec}\": \"{text}\" is not a port"))?;
    if port == 0 {
        bail!("invalid publish value \"{spec}\": ports must be between 1 and 65535");
    }
    Ok(port)
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
        .filter(|m| {
            wanted
                .as_ref()
                .is_none_or(|(port, protocol)| matches_private(m, *port, protocol))
        })
        .collect();
    if let Some((port, protocol)) = &wanted
        && shown.is_empty()
    {
        bail!("No public port '{port}/{protocol}' published for {reference}");
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

fn parse_private_port(text: &str) -> Result<(u16, String)> {
    let (body, protocol) = split_protocol(text)?;
    Ok((parse_port(body, text)?, protocol.to_string()))
}

/// An empty protocol on a mapping reads as tcp.
fn matches_private(mapping: &PortMapping, port: u16, protocol: &str) -> bool {
    let own = if mapping.protocol.is_empty() {
        PROTOCOL
    } else {
        &mapping.protocol
    };
    mapping.container_port == u32::from(port) && own == protocol
}

fn protocol_of(mapping: &PortMapping) -> &str {
    if mapping.protocol.is_empty() {
        PROTOCOL
    } else {
        &mapping.protocol
    }
}

/// `80/tcp -> 127.0.0.1:49153`, or just the address when a port was asked for.
fn mapping_line(mapping: &PortMapping, address_only: bool) -> String {
    let address = host_address(mapping);
    if address_only {
        address
    } else {
        format!(
            "{}/{} -> {address}",
            mapping.container_port,
            protocol_of(mapping)
        )
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
        "protocol": protocol_of(mapping),
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
        let mappings = parse_publish(spec).unwrap();

        // Assert
        let mapping = &mappings[0];
        assert_eq!(mappings.len(), 1);
        assert_eq!(mapping.host_ip, host_ip);
        assert_eq!(mapping.host_port, host_port);
        assert_eq!(mapping.container_port, container_port);
        assert_eq!(mapping.protocol, PROTOCOL);
    }

    #[rstest]
    #[case::sctp("8080:53/sctp", "use tcp or udp")]
    #[case::mismatched_ranges("8000-8010:80-82", "differ in length")]
    #[case::single_host_for_range("8080:80-82", "same length")]
    #[case::backwards("8010-8000:80", "backwards")]
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
            host_port_end: 0,
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
        let mapping = parse_publish(spec).unwrap().remove(0);

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
    #[case::port("80", 80, "tcp")]
    #[case::port_with_protocol("443/tcp", 443, "tcp")]
    #[case::udp("53/udp", 53, "udp")]
    fn given_a_private_port_argument_when_parsed_then_the_port_and_protocol_are_returned(
        #[case] text: &str,
        #[case] port: u16,
        #[case] protocol: &str,
    ) {
        // Act
        let parsed = parse_private_port(text).unwrap();

        // Assert
        assert_eq!(parsed, (port, protocol.to_string()));
    }

    #[rstest]
    #[case::udp_mapping_udp_query("udp", 53, "udp", true)]
    #[case::udp_mapping_tcp_query("udp", 53, "tcp", false)]
    #[case::unset_protocol_is_tcp("", 80, "tcp", true)]
    fn given_a_mapping_when_matched_against_a_private_port_then_the_protocol_counts(
        #[case] own: &str,
        #[case] port: u16,
        #[case] queried: &str,
        #[case] expected: bool,
    ) {
        // Arrange
        let mapping = PortMapping {
            protocol: own.into(),
            container_port: u32::from(port),
            ..mapping("127.0.0.1", 1, u32::from(port))
        };

        // Act
        let matched = matches_private(&mapping, port, queried);

        // Assert
        assert_eq!(matched, expected);
    }

    #[test]
    fn given_a_udp_publish_value_when_parsed_then_the_protocol_is_udp() {
        // Act
        let mappings = parse_publish("5353:53/udp").unwrap();

        // Assert
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].protocol, "udp");
        assert_eq!(
            (mappings[0].host_port, mappings[0].container_port),
            (5353, 53)
        );
    }

    #[test]
    fn given_equal_ranges_when_parsed_then_each_host_port_maps_to_its_container_port() {
        // Act
        let mappings = parse_publish("127.0.0.1:8000-8002:80-82").unwrap();

        // Assert
        let pairs: Vec<_> = mappings
            .iter()
            .map(|m| (m.host_port, m.container_port, m.host_port_end))
            .collect();
        assert_eq!(pairs, [(8000, 80, 0), (8001, 81, 0), (8002, 82, 0)]);
    }

    #[test]
    fn given_a_host_range_and_one_container_port_when_parsed_then_the_range_is_kept() {
        // Act
        let mappings = parse_publish("8000-8010:80").unwrap();

        // Assert
        assert_eq!(mappings.len(), 1);
        assert_eq!(
            (mappings[0].host_port, mappings[0].host_port_end),
            (8000, 8010)
        );
    }

    #[test]
    fn given_a_range_past_the_publish_limit_when_parsed_then_it_is_refused_before_expanding() {
        // Act
        let error = parse_publish("1-65535").unwrap_err();

        // Assert
        assert!(error.to_string().contains("at most 64"), "{error}");
    }

    #[test]
    fn given_a_container_range_only_when_parsed_then_each_port_gets_any_host_port() {
        // Act
        let mappings = parse_publish("80-82").unwrap();

        // Assert
        let ports: Vec<_> = mappings
            .iter()
            .map(|m| (m.host_port, m.container_port))
            .collect();
        assert_eq!(ports, [(0, 80), (0, 81), (0, 82)]);
    }
}
