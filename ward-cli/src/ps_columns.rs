// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The `ps` columns that need more than the sandbox's own fields: PORTS and
//! the SIZE that `-s` adds.

use std::collections::HashMap;

use ward_core::pb::{ImageInfo, SandboxInfo};

use crate::images::size_text;

/// Shown for a figure Ward does not measure.
const UNMEASURED: &str = "--";
/// The protocol of every published port.
const PROTOCOL: &str = "tcp";

/// Docker's PORTS text: `127.0.0.1:8080->80/tcp, 127.0.0.1:8443->443/tcp`.
pub fn ports_text(info: &SandboxInfo) -> String {
    let ports: Vec<String> = info
        .published_ports
        .iter()
        .map(|port| {
            format!(
                "{}:{}->{}/{PROTOCOL}",
                port.host_ip, port.host_port, port.container_port
            )
        })
        .collect();
    ports.join(", ")
}

/// Image sizes by the reference a sandbox was created from.
pub fn image_sizes(images: &[ImageInfo]) -> HashMap<String, u64> {
    images
        .iter()
        .map(|image| (image.reference.clone(), image.size_bytes))
        .collect()
}

/// Docker's `0B (virtual 7.8MB)`. Ward cannot tell how much a sandbox wrote
/// on top of its image, so the first figure is unmeasured.
pub fn size_column(info: &SandboxInfo, sizes: &HashMap<String, u64>) -> String {
    match sizes.get(&info.image) {
        Some(bytes) => format!("{UNMEASURED} (virtual {})", size_text(*bytes)),
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
                    protocol: PROTOCOL.into(),
                },
                PortMapping {
                    host_ip: "0.0.0.0".into(),
                    host_port: 8443,
                    container_port: 443,
                    protocol: PROTOCOL.into(),
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
