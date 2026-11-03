// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The `ps` columns that need more than the sandbox's own fields: PORTS and
//! the SIZE that `-s` adds.

use std::collections::HashMap;

use ward_core::pb::{ImageInfo, SandboxInfo};

use crate::images::size_text;

/// Shown for a figure Ward does not measure.
const UNMEASURED: &str = "--";
/// The protocol of a port that names none.
const DEFAULT_PROTOCOL: &str = "tcp";
const DEFAULT_TAG: &str = "latest";
const DIGEST_PREFIX: &str = "sha256:";
/// Shortest digest or ID prefix that names an image.
const MIN_ID_PREFIX: usize = 4;

/// A run of ports on one host address that Docker prints as one range.
struct PortRun<'a> {
    host_ip: &'a str,
    protocol: &'a str,
    host: (u32, u32),
    container: (u32, u32),
}

impl PortRun<'_> {
    fn text(&self) -> String {
        let span = |(first, last): (u32, u32)| {
            if first == last {
                first.to_string()
            } else {
                format!("{first}-{last}")
            }
        };
        format!(
            "{}:{}->{}/{}",
            self.host_ip,
            span(self.host),
            span(self.container),
            self.protocol
        )
    }
}

/// Docker's PORTS text: `127.0.0.1:8080->80/tcp, 127.0.0.1:8443->443/tcp`.
/// Ports are ordered by container port, and consecutive ones on the same
/// address and protocol print as a range, as `docker ps` does.
pub fn ports_text(info: &SandboxInfo) -> String {
    let mut ports: Vec<_> = info.published_ports.iter().collect();
    ports.sort_by(|a, b| {
        (a.container_port, a.host_port, &a.host_ip).cmp(&(
            b.container_port,
            b.host_port,
            &b.host_ip,
        ))
    });
    let mut runs: Vec<PortRun> = Vec::new();
    for port in ports {
        let protocol = if port.protocol.is_empty() {
            DEFAULT_PROTOCOL
        } else {
            port.protocol.as_str()
        };
        match runs.last_mut() {
            Some(run)
                if run.host_ip == port.host_ip
                    && run.protocol == protocol
                    && run.host.1 + 1 == port.host_port
                    && run.container.1 + 1 == port.container_port =>
            {
                run.host.1 = port.host_port;
                run.container.1 = port.container_port;
            }
            _ => runs.push(PortRun {
                host_ip: &port.host_ip,
                protocol,
                host: (port.host_port, port.host_port),
                container: (port.container_port, port.container_port),
            }),
        }
    }
    runs.iter()
        .map(PortRun::text)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Image sizes by every name a cached image answers to: its reference and its
/// digest.
pub fn image_sizes(images: &[ImageInfo]) -> HashMap<String, u64> {
    images
        .iter()
        .flat_map(|image| {
            [
                (image.reference.clone(), image.size_bytes),
                (image.digest.clone(), image.size_bytes),
            ]
        })
        .collect()
}

/// The size of the image a sandbox was created from. The digest it resolved to
/// is exact; otherwise the reference as written, then with the default tag,
/// then as a digest or ID prefix.
fn image_size(info: &SandboxInfo, sizes: &HashMap<String, u64>) -> Option<u64> {
    let by_name = |name: &str| sizes.get(name).copied();
    let tagged = format!("{}:{DEFAULT_TAG}", info.image);
    let by_prefix = || {
        let wanted = info
            .image
            .strip_prefix(DIGEST_PREFIX)
            .unwrap_or(&info.image);
        if wanted.len() < MIN_ID_PREFIX || !wanted.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        sizes
            .iter()
            .filter(|(name, _)| {
                name.strip_prefix(DIGEST_PREFIX)
                    .is_some_and(|hex| hex.starts_with(wanted))
            })
            .map(|(_, bytes)| *bytes)
            .next()
    };
    Some(info.image_digest.as_str())
        .filter(|digest| !digest.is_empty())
        .and_then(by_name)
        .or_else(|| by_name(&info.image))
        .or_else(|| by_name(&tagged))
        .or_else(by_prefix)
}

/// Whether a sandbox was made from the cached image, resolving its image the
/// way `image_size` does.
pub fn uses_image(info: &SandboxInfo, image: &ImageInfo) -> bool {
    if !info.image_digest.is_empty() && info.image_digest == image.digest {
        return true;
    }
    if info.image == image.reference || format!("{}:{DEFAULT_TAG}", info.image) == image.reference {
        return true;
    }
    let wanted = info
        .image
        .strip_prefix(DIGEST_PREFIX)
        .unwrap_or(&info.image);
    wanted.len() >= MIN_ID_PREFIX
        && wanted.chars().all(|c| c.is_ascii_hexdigit())
        && image
            .digest
            .strip_prefix(DIGEST_PREFIX)
            .is_some_and(|hex| hex.starts_with(wanted))
}

/// Docker's `0B (virtual 7.8MB)`. Ward cannot tell how much a sandbox wrote
/// on top of its image, so the first figure is unmeasured.
pub fn size_column(info: &SandboxInfo, sizes: &HashMap<String, u64>) -> String {
    match image_size(info, sizes) {
        Some(bytes) => format!("{UNMEASURED} (virtual {})", size_text(bytes)),
        None => UNMEASURED.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ward_core::pb::PortMapping;

    fn sandbox(image: &str) -> SandboxInfo {
        SandboxInfo {
            image: image.into(),
            published_ports: vec![
                PortMapping {
                    host_ip: "127.0.0.1".into(),
                    host_port: 8080,
                    container_port: 80,
                    protocol: "tcp".into(),
                },
                PortMapping {
                    host_ip: "0.0.0.0".into(),
                    host_port: 8443,
                    container_port: 443,
                    protocol: "tcp".into(),
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn given_published_ports_when_listed_then_they_read_like_docker_ps() {
        assert_eq!(
            ports_text(&sandbox("alpine")),
            "127.0.0.1:8080->80/tcp, 0.0.0.0:8443->443/tcp"
        );
    }

    fn port(host_ip: &str, host_port: u32, container_port: u32, protocol: &str) -> PortMapping {
        PortMapping {
            host_ip: host_ip.into(),
            host_port,
            container_port,
            protocol: protocol.into(),
        }
    }

    #[test]
    fn given_an_empty_protocol_when_listed_then_it_reads_as_tcp() {
        let info = SandboxInfo {
            published_ports: vec![port("0.0.0.0", 8080, 80, "")],
            ..Default::default()
        };

        assert_eq!(ports_text(&info), "0.0.0.0:8080->80/tcp");
    }

    #[test]
    fn given_another_protocol_when_listed_then_it_is_shown() {
        let info = SandboxInfo {
            published_ports: vec![port("0.0.0.0", 53, 53, "udp")],
            ..Default::default()
        };

        assert_eq!(ports_text(&info), "0.0.0.0:53->53/udp");
    }

    #[test]
    fn given_unsorted_consecutive_ports_when_listed_then_they_collapse_into_ranges() {
        let info = SandboxInfo {
            published_ports: vec![
                port("0.0.0.0", 9000, 9000, "tcp"),
                port("0.0.0.0", 8002, 8002, "tcp"),
                port("0.0.0.0", 8000, 8000, "tcp"),
                port("0.0.0.0", 8001, 8001, "tcp"),
                port("127.0.0.1", 8003, 8003, "tcp"),
            ],
            ..Default::default()
        };

        assert_eq!(
            ports_text(&info),
            "0.0.0.0:8000-8002->8000-8002/tcp, 127.0.0.1:8003->8003/tcp, 0.0.0.0:9000->9000/tcp"
        );
    }

    #[test]
    fn given_ports_that_are_only_consecutive_on_one_side_when_listed_then_they_stay_apart() {
        let info = SandboxInfo {
            published_ports: vec![
                port("0.0.0.0", 8000, 80, "tcp"),
                port("0.0.0.0", 8005, 81, "tcp"),
            ],
            ..Default::default()
        };

        assert_eq!(
            ports_text(&info),
            "0.0.0.0:8000->80/tcp, 0.0.0.0:8005->81/tcp"
        );
    }

    #[rstest::rstest]
    #[case::default_tag("alpine", "")]
    #[case::exact("alpine:latest", "")]
    #[case::digest("whatever", "sha256:abcdef0123")]
    #[case::digest_prefix("abcdef", "")]
    #[case::full_digest_with_prefix("sha256:abcdef01", "")]
    fn given_a_cached_tagged_image_when_sized_then_the_reference_resolves_like_the_daemon(
        #[case] image: &str,
        #[case] digest: &str,
    ) {
        let sizes = image_sizes(&[ImageInfo {
            reference: "alpine:latest".into(),
            digest: "sha256:abcdef0123".into(),
            size_bytes: 7_800_000,
            ..Default::default()
        }]);
        let info = SandboxInfo {
            image: image.into(),
            image_digest: digest.into(),
            ..Default::default()
        };

        assert_eq!(size_column(&info, &sizes), "-- (virtual 7.8MB)");
    }

    #[test]
    fn given_a_short_or_unknown_prefix_when_sized_then_nothing_is_guessed() {
        let sizes = image_sizes(&[ImageInfo {
            reference: "alpine:latest".into(),
            digest: "sha256:abcdef0123".into(),
            size_bytes: 1,
            ..Default::default()
        }]);

        for image in ["abc", "ffff", "alpine:3"] {
            let info = SandboxInfo {
                image: image.into(),
                ..Default::default()
            };
            assert_eq!(size_column(&info, &sizes), "--", "{image}");
        }
    }

    #[test]
    fn given_no_ports_when_listed_then_the_text_is_empty() {
        assert_eq!(ports_text(&SandboxInfo::default()), "");
    }

    #[test]
    fn given_a_cached_image_when_sized_then_the_virtual_size_is_shown() {
        let sizes = image_sizes(&[ImageInfo {
            reference: "alpine".into(),
            size_bytes: 7_800_000,
            ..Default::default()
        }]);

        assert_eq!(
            size_column(&sandbox("alpine"), &sizes),
            "-- (virtual 7.8MB)"
        );
        assert_eq!(size_column(&sandbox("other"), &sizes), "--");
    }
}
