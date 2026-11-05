// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward system df -v`: the per-object usage tables of `docker system df -v`.

use anyhow::{Result, bail};
use chrono::Utc;
use ward_core::pb::{ImageInfo, SandboxInfo, VolumeInfo};

use crate::client;
use crate::images::{BYTES_PER_MIB, record as image_record, size_text};
use crate::inspect::ps_json_object;
use crate::ps_columns::uses_image;
use crate::volume::link_counts;

const COLUMN_PADDING: usize = 3;
/// Shown for a figure Ward does not measure.
const UNMEASURED: &str = "--";
/// What `docker system df -v` prints for the build cache; Ward has none.
const NO_BUILD_CACHE: &str = "Build cache usage: 0B";
const IMAGE_HEADERS: [&str; 8] = [
    "REPOSITORY",
    "TAG",
    "IMAGE ID",
    "CREATED",
    "SIZE",
    "SHARED SIZE",
    "UNIQUE SIZE",
    "CONTAINERS",
];
const CONTAINER_HEADERS: [&str; 8] = [
    "CONTAINER ID",
    "IMAGE",
    "COMMAND",
    "LOCAL VOLUMES",
    "SIZE",
    "CREATED",
    "STATUS",
    "NAMES",
];
const VOLUME_HEADERS: [&str; 3] = ["VOLUME NAME", "LINKS", "SIZE"];

type Row = Vec<String>;

/// `headers` and `rows` as columns padded to their widest cell. The last
/// column is not padded.
fn table(headers: &[&str], rows: &[Row]) -> String {
    let header: Row = headers.iter().map(ToString::to_string).collect();
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            std::iter::once(&header)
                .chain(rows)
                .map(|line| line[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for line in std::iter::once(&header).chain(rows) {
        for (column, cell) in line.iter().enumerate() {
            out.push_str(cell);
            if column + 1 < line.len() {
                let pad = widths[column] + COLUMN_PADDING - cell.chars().count();
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push('\n');
    }
    out
}

/// How many sandboxes were made from the image, by any name that reaches it.
fn users_of(image: &ImageInfo, sandboxes: &[SandboxInfo]) -> usize {
    sandboxes.iter().filter(|s| uses_image(s, image)).count()
}

fn image_rows(images: &[ImageInfo], sandboxes: &[SandboxInfo]) -> Vec<Row> {
    let now = Utc::now();
    images
        .iter()
        .map(|image| {
            let record = image_record(image, false, now);
            let field = |name: &str| record[name].as_str().unwrap_or_default().to_string();
            vec![
                field("Repository"),
                field("Tag"),
                field("ID"),
                field("CreatedSince"),
                field("Size"),
                // Each image is one flattened layer, so nothing is shared.
                size_text(0),
                size_text(image.size_bytes),
                users_of(image, sandboxes).to_string(),
            ]
        })
        .collect()
}

fn container_rows(sandboxes: &[SandboxInfo]) -> Vec<Row> {
    let now = Utc::now().timestamp();
    sandboxes
        .iter()
        .map(|info| {
            let object = ps_json_object(info, now);
            let field = |name: &str| object[name].as_str().unwrap_or_default().to_string();
            vec![
                field("ID"),
                field("Image"),
                field("Command"),
                info.mounts
                    .iter()
                    .filter(|m| !m.volume.is_empty())
                    .count()
                    .to_string(),
                UNMEASURED.to_string(),
                field("RunningFor"),
                field("Status"),
                field("Names"),
            ]
        })
        .collect()
}

fn volume_rows(volumes: &[VolumeInfo], sandboxes: &[SandboxInfo]) -> Vec<Row> {
    let links = link_counts(sandboxes);
    volumes
        .iter()
        .map(|volume| {
            vec![
                volume.name.clone(),
                links
                    .get(&volume.name)
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
                size_text(u64::from(volume.size_mb) * BYTES_PER_MIB),
            ]
        })
        .collect()
}

/// The text of the four sections.
fn report(images: &[ImageInfo], sandboxes: &[SandboxInfo], volumes: &[VolumeInfo]) -> String {
    format!(
        "Images space usage:\n\n{}\nContainers space usage:\n\n{}\nLocal Volumes space usage:\n\n{}\n{NO_BUILD_CACHE}\n",
        table(&IMAGE_HEADERS, &image_rows(images, sandboxes)),
        table(&CONTAINER_HEADERS, &container_rows(sandboxes)),
        table(&VOLUME_HEADERS, &volume_rows(volumes, sandboxes)),
    )
}

/// # Errors
///
/// Fails with `--json`, which has no verbose form, or when the daemon cannot
/// be reached.
pub async fn run(socket: &str, json: bool) -> Result<()> {
    if json {
        bail!("--json is not supported with system df -v");
    }
    let mut c = client::connect(socket).await?;
    let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let images = c.list_images(()).await?.into_inner().images;
    let volumes = c.list_volumes(()).await?.into_inner().volumes;
    print!("{}", report(&images, &sandboxes, &volumes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(reference: &str, digest: &str, size_bytes: u64) -> ImageInfo {
        ImageInfo {
            reference: reference.into(),
            digest: digest.into(),
            size_bytes,
            ..Default::default()
        }
    }

    #[test]
    fn given_rows_when_tabled_then_columns_align_and_the_last_is_unpadded() {
        let rows = vec![vec!["a".to_string(), "longer".to_string()]];

        let text = table(&["ID", "NAME"], &rows);

        assert_eq!(text, "ID   NAME\na    longer\n");
    }

    #[test]
    fn given_images_sandboxes_and_volumes_when_reported_then_all_sections_print() {
        let images = [image("alpine:3", "sha256:aaaaaaaaaaaaaaaa", 7_800_000)];
        let sandboxes = [SandboxInfo {
            id: "0123456789abcdef".into(),
            name: "web1".into(),
            image: "alpine:3".into(),
            ..Default::default()
        }];
        let volumes = [VolumeInfo {
            name: "data".into(),
            size_mb: 100,
            ..Default::default()
        }];

        let text = report(&images, &sandboxes, &volumes);

        let titles: Vec<&str> = text.lines().filter(|l| l.ends_with("usage:")).collect();
        assert_eq!(
            titles,
            [
                "Images space usage:",
                "Containers space usage:",
                "Local Volumes space usage:"
            ]
        );
        assert!(text.contains("alpine       3     aaaaaaaaaaaa"), "{text}");
        assert!(
            text.contains("7.8MB   0B            7.8MB         1"),
            "{text}"
        );
        assert!(text.contains("0123456789ab   alpine:3"), "{text}");
        assert!(text.contains("data          0       104.9MB"), "{text}");
        assert!(text.ends_with("Build cache usage: 0B\n"), "{text}");
    }

    #[test]
    fn given_an_untagged_sandbox_image_when_users_counted_then_the_default_tag_matches() {
        let cached = image("alpine:latest", "sha256:abcdef0123", 1);
        let sandboxes = [
            SandboxInfo {
                image: "alpine".into(),
                ..Default::default()
            },
            SandboxInfo {
                image: "abcdef".into(),
                ..Default::default()
            },
            SandboxInfo {
                image: "alpine:3".into(),
                ..Default::default()
            },
        ];

        assert_eq!(users_of(&cached, &sandboxes), 2);
    }

    #[test]
    fn given_an_image_when_users_counted_then_tag_and_digest_both_count() {
        let cached = image("alpine:3", "sha256:a", 1);
        let sandboxes = [
            SandboxInfo {
                image: "alpine:3".into(),
                ..Default::default()
            },
            SandboxInfo {
                image: "other".into(),
                image_digest: "sha256:a".into(),
                ..Default::default()
            },
            SandboxInfo {
                image: "other".into(),
                ..Default::default()
            },
        ];

        assert_eq!(users_of(&cached, &sandboxes), 2);
    }
}
