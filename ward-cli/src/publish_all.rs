// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `-P/--publish-all` and `--expose` of `ward run` and `ward create`.
//!
//! `--expose` names a port the sandbox listens on and publishes nothing by
//! itself. `-P` publishes every exposed port on a free host port.

use anyhow::{Result, bail};
use clap::Args;
use ward_core::pb::PortMapping;

use crate::ports::parse_publish;

const HOST_PORT_SEPARATOR: char = ':';

/// Why `-P` has nothing to publish. Ward does not read the image's EXPOSE.
const NOTHING_TO_PUBLISH: &str = "-P has no ports to publish: Ward does not read the image's EXPOSE, so name them with --expose PORT or use -p";

#[derive(Debug, Args, Default)]
pub struct PublishAllFlags {
    /// Publish every port named by --expose on a free host port, bound to
    /// 127.0.0.1. Ward does not read the image's EXPOSE.
    #[arg(short = 'P', long = "publish-all")]
    publish_all: bool,
    /// Name a port the sandbox listens on: PORT[-PORT][/tcp|/udp]. It is published
    /// only with -P. Repeatable.
    #[arg(long, value_name = "PORT[-PORT][/tcp|/udp]")]
    expose: Vec<String>,
}

impl PublishAllFlags {
    /// The mappings `-P` adds to the ones `-p` gave: one per exposed port
    /// that `-p` did not already publish. Empty without `-P`.
    ///
    /// # Errors
    ///
    /// Fails on a malformed or unsupported `--expose` value, and on `-P`
    /// when no port is exposed.
    pub fn mappings(&self, published: &[PortMapping]) -> Result<Vec<PortMapping>> {
        let exposed = self
            .expose
            .iter()
            .map(|spec| parse_expose(spec))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !self.publish_all {
            return Ok(Vec::new());
        }
        if exposed.is_empty() {
            bail!(NOTHING_TO_PUBLISH);
        }
        let mut added: Vec<PortMapping> = Vec::new();
        for mapping in exposed {
            let taken = published
                .iter()
                .chain(&added)
                .any(|m| same_port(m, &mapping));
            if !taken {
                added.push(mapping);
            }
        }
        Ok(added)
    }
}

/// True when both map the same container port and protocol.
fn same_port(a: &PortMapping, b: &PortMapping) -> bool {
    a.container_port == b.container_port && a.protocol == b.protocol
}

/// An `--expose` value is a container port or range with no host side.
fn parse_expose(spec: &str) -> Result<Vec<PortMapping>> {
    if spec.contains(HOST_PORT_SEPARATOR) {
        bail!("invalid --expose value \"{spec}\": expected PORT[-PORT][/tcp|/udp]");
    }
    parse_publish(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(publish_all: bool, expose: &[&str]) -> PublishAllFlags {
        PublishAllFlags {
            publish_all,
            expose: expose.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn mapped(port: u32) -> PortMapping {
        PortMapping {
            container_port: port,
            protocol: "tcp".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn given_expose_without_publish_all_when_mapped_then_nothing_is_published() {
        let added = flags(false, &["80"]).mappings(&[]).unwrap();

        assert!(added.is_empty());
    }

    #[test]
    fn given_publish_all_with_expose_when_mapped_then_each_port_gets_a_free_host_port() {
        let added = flags(true, &["80", "443/tcp"]).mappings(&[]).unwrap();

        let ports: Vec<_> = added
            .iter()
            .map(|m| (m.container_port, m.host_port))
            .collect();
        assert_eq!(ports, [(80, 0), (443, 0)]);
    }

    #[test]
    fn given_a_udp_port_and_a_range_when_mapped_then_each_port_keeps_its_protocol() {
        let added = flags(true, &["53/udp", "8000-8001"]).mappings(&[]).unwrap();

        let ports: Vec<_> = added
            .iter()
            .map(|m| (m.container_port, m.protocol.as_str()))
            .collect();
        assert_eq!(ports, [(53, "udp"), (8000, "tcp"), (8001, "tcp")]);
    }

    #[test]
    fn given_a_port_already_published_when_mapped_then_it_is_not_added_twice() {
        let added = flags(true, &["80", "80", "81"])
            .mappings(&[mapped(81)])
            .unwrap();

        let ports: Vec<_> = added.iter().map(|m| m.container_port).collect();
        assert_eq!(ports, [80]);
    }

    #[test]
    fn given_publish_all_without_expose_when_mapped_then_the_error_says_why() {
        let error = flags(true, &[]).mappings(&[]).unwrap_err().to_string();

        assert_eq!(error, NOTHING_TO_PUBLISH);
        assert_eq!(error.lines().count(), 1);
    }

    #[rstest::rstest]
    #[case::host_side("8080:80")]
    #[case::protocol("53/sctp")]
    #[case::text("web")]
    fn given_a_bad_expose_value_when_mapped_then_it_fails(#[case] spec: &str) {
        assert!(flags(false, &[spec]).mappings(&[]).is_err());
    }
}
