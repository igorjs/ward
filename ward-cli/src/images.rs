// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward images`, laid out like `docker images`.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use ward_core::pb::ImageInfo;

use crate::client;
use crate::inspect::{duration_text, seconds_since};
use crate::stats::human_size;
use crate::template::Template;

const SHORT_ID_LEN: usize = 12;
const DIGEST_PREFIX: &str = "sha256:";
const DEFAULT_TAG: &str = "latest";
/// Shown for a reference with no tag, as Docker does for a digest reference.
const NO_TAG: &str = "<none>";
/// Docker prints image sizes in decimal units.
const BYTES_PER_UNIT: f64 = 1000.0;
const SIZE_UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
const DEFAULT_FORMAT: &str =
    "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}";
const CREATED_AT_FORMAT: &str = "%Y-%m-%d %H:%M:%S %z UTC";
const FILTER_REFERENCE: &str = "reference";
const FILTER_DANGLING: &str = "dangling";
const WILDCARD: char = '*';
/// Each `--format` field with the header a `table` template gives it.
const FIELDS: [(&str, &str); 7] = [
    ("Repository", "REPOSITORY"),
    ("Tag", "TAG"),
    ("ID", "IMAGE ID"),
    ("Digest", "DIGEST"),
    ("CreatedAt", "CREATED AT"),
    ("CreatedSince", "CREATED"),
    ("Size", "SIZE"),
];

pub struct Options {
    pub quiet: bool,
    pub no_trunc: bool,
    pub format: Option<String>,
    pub filters: Vec<String>,
    /// `REPOSITORY[:TAG]`.
    pub reference: Option<String>,
    pub json: bool,
}

/// Prints the cached images that pass the filters.
pub async fn run(socket: &str, options: Options) -> Result<()> {
    let filters = Filters::parse(&options.filters, options.reference.as_deref())?;
    let template = Template::parse(options.format.as_deref().unwrap_or(DEFAULT_FORMAT), &FIELDS)?;
    let mut c = client::connect(socket).await?;
    let images = c.list_images(()).await?.into_inner().images;
    let now = Utc::now();
    let records: Vec<Map<String, Value>> = images
        .iter()
        .filter(|image| filters.allows(&image.reference))
        .map(|image| record(image, options.no_trunc, now))
        .collect();
    if options.quiet {
        for record in &records {
            println!("{}", record["ID"].as_str().unwrap_or_default());
        }
    } else if options.json && options.format.is_none() {
        println!(
            "{}",
            Value::Array(records.into_iter().map(Value::Object).collect())
        );
    } else {
        print!("{}", template.render(&records));
    }
    Ok(())
}

/// `--filter` values and the positional reference. Different keys all must
/// match; values of one key are alternatives.
struct Filters {
    references: Vec<String>,
    /// Only `dangling=true` matters: cached images are always tagged.
    dangling: Option<bool>,
}

impl Filters {
    fn parse(filters: &[String], repository: Option<&str>) -> Result<Self> {
        let mut parsed = Self {
            references: repository.map(String::from).into_iter().collect(),
            dangling: None,
        };
        for filter in filters {
            match filter.split_once('=') {
                Some((FILTER_REFERENCE, value)) => parsed.references.push(value.to_string()),
                Some((FILTER_DANGLING, "true")) => parsed.dangling = Some(true),
                Some((FILTER_DANGLING, "false")) => parsed.dangling = Some(false),
                Some((key, _)) => bail!("Invalid filter '{key}'"),
                None => bail!("Bad format of filter (expected name=value)"),
            }
        }
        Ok(parsed)
    }

    fn allows(&self, reference: &str) -> bool {
        let (repository, tag) = split_reference(reference);
        self.dangling != Some(true)
            && (self.references.is_empty()
                || self
                    .references
                    .iter()
                    .any(|wanted| matches_reference(wanted, reference, repository, tag)))
    }
}

/// A wanted `repository`, `repository:tag` or wildcard pattern against an image.
fn matches_reference(wanted: &str, reference: &str, repository: &str, tag: &str) -> bool {
    if wanted.contains(WILDCARD) {
        return wildcard_match(wanted, reference) || wildcard_match(wanted, repository);
    }
    match wanted
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
    {
        Some((wanted_repository, wanted_tag)) => {
            wanted_repository == repository && wanted_tag == tag
        }
        None => wanted == repository,
    }
}

/// `*` stands for any run of characters.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let Some((head, rest)) = pattern.split_once(WILDCARD) else {
        return pattern == text;
    };
    let Some(tail) = text.strip_prefix(head) else {
        return false;
    };
    (0..=tail.len())
        .filter(|&at| tail.is_char_boundary(at))
        .any(|at| wildcard_match(rest, &tail[at..]))
}

/// Splits `registry/name:tag` into its repository and tag. A reference with
/// no tag is `latest`, and one pinned by digest has none.
fn split_reference(reference: &str) -> (&str, &str) {
    if let Some((repository, _)) = reference.split_once('@') {
        return (repository, NO_TAG);
    }
    match reference
        .rsplit_once(':')
        .filter(|(_, tag)| !tag.contains('/'))
    {
        Some((repository, tag)) => (repository, tag),
        None => (reference, DEFAULT_TAG),
    }
}

fn record(image: &ImageInfo, no_trunc: bool, now: DateTime<Utc>) -> Map<String, Value> {
    let (repository, tag) = split_reference(&image.reference);
    let id = if no_trunc {
        image.digest.clone()
    } else {
        image
            .digest
            .strip_prefix(DIGEST_PREFIX)
            .unwrap_or(&image.digest)
            .chars()
            .take(SHORT_ID_LEN)
            .collect()
    };
    let created = image.created_at.as_ref();
    let created_at = created
        .and_then(|t| DateTime::from_timestamp(t.seconds, 0))
        .map(|t| t.format(CREATED_AT_FORMAT).to_string())
        .unwrap_or_default();
    let since = seconds_since(&image.created_at, now.timestamp())
        .map(|seconds| format!("{} ago", duration_text(seconds)))
        .unwrap_or_default();
    match json!({
        "Repository": repository,
        "Tag": tag,
        "ID": id,
        "Digest": image.digest,
        "CreatedAt": created_at,
        "CreatedSince": since,
        "Size": human_size(image.size_bytes, BYTES_PER_UNIT, &SIZE_UNITS),
    }) {
        Value::Object(map) => map,
        _ => unreachable!("json! of an object literal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(reference: &str, size_bytes: u64) -> ImageInfo {
        ImageInfo {
            reference: reference.to_string(),
            digest: format!("{DIGEST_PREFIX}{}", "ab".repeat(32)),
            size_bytes,
            created_at: Some(prost_types::Timestamp {
                seconds: 0,
                nanos: 0,
            }),
        }
    }

    #[rstest::rstest]
    #[case::bare("alpine", "alpine", "latest")]
    #[case::tagged("alpine:3.20", "alpine", "3.20")]
    #[case::registry_port_tag("localhost:5000/app:v1", "localhost:5000/app", "v1")]
    #[case::digest("alpine@sha256:abc", "alpine", "<none>")]
    fn given_a_reference_when_split_then_repository_and_tag_are_docker_style(
        #[case] reference: &str,
        #[case] repository: &str,
        #[case] tag: &str,
    ) {
        assert_eq!(split_reference(reference), (repository, tag));
    }

    #[test]
    fn given_an_image_when_rendered_by_default_then_it_has_docker_columns() {
        // Arrange
        let records = vec![record(&image("alpine:3", 7_800_000), false, Utc::now())];
        let template = Template::parse(DEFAULT_FORMAT, &FIELDS).expect("parse");

        // Act
        let out = template.render(&records);

        // Assert
        let mut lines = out.lines();
        let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
        assert_eq!(
            header,
            ["REPOSITORY", "TAG", "IMAGE", "ID", "CREATED", "SIZE"]
        );
        let row = lines.next().unwrap();
        assert!(row.contains("alpine"), "{row}");
        assert!(row.contains("abababababab"), "{row}");
        assert!(row.ends_with("7.8MB"), "{row}");
        assert!(row.contains("days ago"), "{row}");
    }

    #[rstest::rstest]
    #[case::repository("alpine", "alpine:3", true)]
    #[case::other_repository("busybox", "alpine:3", false)]
    #[case::tag_match("alpine:3", "alpine:3", true)]
    #[case::tag_mismatch("alpine:4", "alpine:3", false)]
    #[case::default_tag("alpine:latest", "alpine", true)]
    #[case::wildcard("alp*", "alpine:3", true)]
    fn given_a_wanted_reference_when_matched_then_repository_and_tag_decide(
        #[case] wanted: &str,
        #[case] reference: &str,
        #[case] expected: bool,
    ) {
        let filters = Filters::parse(&[], Some(wanted)).expect("parse");

        assert_eq!(filters.allows(reference), expected);
    }
}
