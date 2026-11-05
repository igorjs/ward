// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward images`, laid out like `docker images`.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use ward_core::pb::ImageInfo;

use crate::client;
use crate::exists::names_image;
use crate::inspect::{duration_text, seconds_since};
use crate::stats::human_size;
use crate::template::Template;

pub(crate) const SHORT_ID_LEN: usize = 12;
pub(crate) const DIGEST_PREFIX: &str = "sha256:";
const DEFAULT_TAG: &str = "latest";
/// Shown for a reference with no tag, as Docker does for a digest reference.
const NO_TAG: &str = "<none>";
/// Docker prints image sizes in decimal units.
pub(crate) const BYTES_PER_UNIT: f64 = 1000.0;
pub(crate) const SIZE_UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
const DEFAULT_FORMAT: &str =
    "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}";
pub(crate) const CREATED_AT_FORMAT: &str = "%Y-%m-%d %H:%M:%S %z UTC";
const FILTER_REFERENCE: &str = "reference";
const FILTER_DANGLING: &str = "dangling";
const FILTER_BEFORE: &str = "before";
const FILTER_SINCE: &str = "since";
const FILTER_LABEL: &str = "label";
const DIGESTS_FORMAT: &str =
    "table {{.Repository}}\t{{.Tag}}\t{{.Digest}}\t{{.ID}}\t{{.CreatedSince}}\t{{.Size}}";
const NEGATION_MARK: char = '!';
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

pub const BYTES_PER_MIB: u64 = 1024 * 1024;

/// A byte count in Docker's decimal units.
pub fn size_text(bytes: u64) -> String {
    human_size(bytes, BYTES_PER_UNIT, &SIZE_UNITS)
}

pub struct Options {
    pub quiet: bool,
    pub no_trunc: bool,
    pub format: Option<String>,
    pub filters: Vec<String>,
    /// `REPOSITORY[:TAG]`.
    pub reference: Option<String>,
    /// `--digests`: add the DIGEST column.
    pub digests: bool,
    pub json: bool,
}

/// Prints the cached images that pass the filters.
pub async fn run(socket: &str, options: Options) -> Result<()> {
    let mut filters = Filters::parse(&options.filters, options.reference.as_deref())?;
    let default_format = if options.digests {
        DIGESTS_FORMAT
    } else {
        DEFAULT_FORMAT
    };
    let template = Template::parse(options.format.as_deref().unwrap_or(default_format), &FIELDS)?;
    let mut c = client::connect(socket).await?;
    let images = c.list_images(()).await?.into_inner().images;
    filters.resolve(&images)?;
    let now = Utc::now();
    let records: Vec<Map<String, Value>> = images
        .iter()
        .filter(|image| filters.allows(image))
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
        print!("{}", template.render(&records)?);
    }
    Ok(())
}

/// `--filter` values and the positional reference. Different keys all must
/// match; values of one key are alternatives.
struct Filters {
    references: Vec<String>,
    /// Only `dangling=true` matters: cached images are always tagged.
    dangling: Option<bool>,
    /// Image named by the last `before=`; others must be older.
    before: Option<String>,
    since: Option<String>,
    before_at: Option<i64>,
    since_at: Option<i64>,
    /// Images carry no labels, so `label=` matches none and `label!=` all.
    label_wanted: bool,
}

impl Filters {
    fn parse(filters: &[String], repository: Option<&str>) -> Result<Self> {
        let mut parsed = Self {
            references: repository.map(String::from).into_iter().collect(),
            dangling: None,
            before: None,
            since: None,
            before_at: None,
            since_at: None,
            label_wanted: false,
        };
        for filter in filters {
            match filter.split_once('=') {
                Some((FILTER_REFERENCE, value)) => parsed.references.push(value.to_string()),
                Some((FILTER_DANGLING, "true")) => parsed.dangling = Some(true),
                Some((FILTER_DANGLING, "false")) => parsed.dangling = Some(false),
                Some((FILTER_BEFORE, value)) if !value.is_empty() => {
                    parsed.before = Some(value.to_string());
                }
                Some((FILTER_SINCE, value)) if !value.is_empty() => {
                    parsed.since = Some(value.to_string());
                }
                Some((FILTER_LABEL, value)) if !value.is_empty() => parsed.label_wanted = true,
                Some((key @ (FILTER_BEFORE | FILTER_SINCE | FILTER_LABEL), "")) => {
                    bail!("invalid filter '{key}=': a value is required")
                }
                Some((key, _)) if key.strip_suffix(NEGATION_MARK) == Some(FILTER_LABEL) => {}
                Some((key, _)) => bail!("Invalid filter '{key}'"),
                None => bail!("Bad format of filter (expected name=value)"),
            }
        }
        Ok(parsed)
    }

    /// Looks up the images `before=` and `since=` name.
    ///
    /// # Errors
    ///
    /// Fails when one of them names no cached image.
    fn resolve(&mut self, images: &[ImageInfo]) -> Result<()> {
        let created_of = |name: &Option<String>| -> Result<Option<i64>> {
            let Some(name) = name else { return Ok(None) };
            match images.iter().find(|image| names_image(image, name)) {
                Some(image) => Ok(Some(created_seconds(image))),
                None => bail!("No such image: {name}"),
            }
        };
        self.before_at = created_of(&self.before)?;
        self.since_at = created_of(&self.since)?;
        Ok(())
    }

    fn allows(&self, image: &ImageInfo) -> bool {
        let reference = &image.reference;
        let (repository, tag) = split_reference(reference);
        self.dangling != Some(true)
            && !self.label_wanted
            && self.before_at.is_none_or(|at| created_seconds(image) < at)
            && self.since_at.is_none_or(|at| created_seconds(image) > at)
            && (self.references.is_empty()
                || self
                    .references
                    .iter()
                    .any(|wanted| matches_reference(wanted, reference, repository, tag)))
    }
}

fn created_seconds(image: &ImageInfo) -> i64 {
    image.created_at.as_ref().map_or(0, |t| t.seconds)
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

pub(crate) fn record(image: &ImageInfo, no_trunc: bool, now: DateTime<Utc>) -> Map<String, Value> {
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
        "Size": size_text(image.size_bytes),
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
        let out = template.render(&records).unwrap();

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

        assert_eq!(filters.allows(&image(reference, 1)), expected);
    }

    fn dated(reference: &str, seconds: i64) -> ImageInfo {
        let mut image = image(reference, 1);
        image.created_at = Some(prost_types::Timestamp { seconds, nanos: 0 });
        image
    }

    fn listed_after(filter: &str) -> Vec<String> {
        let images = [dated("old:1", 10), dated("mid:1", 20), dated("new:1", 30)];
        let mut filters = Filters::parse(&[filter.to_string()], None).expect("parse");
        filters.resolve(&images).expect("resolve");
        images
            .iter()
            .filter(|i| filters.allows(i))
            .map(|i| i.reference.clone())
            .collect()
    }

    #[rstest::rstest]
    #[case::before("before=mid:1", &["old:1"])]
    #[case::since("since=mid:1", &["new:1"])]
    #[case::since_old("since=old:1", &["mid:1", "new:1"])]
    #[case::label_matches_none("label=env=dev", &[])]
    #[case::label_refused_matches_all("label!=env=dev", &["old:1", "mid:1", "new:1"])]
    fn given_a_relative_or_label_filter_when_listed_then_creation_time_decides(
        #[case] filter: &str,
        #[case] expected: &[&str],
    ) {
        assert_eq!(listed_after(filter), expected);
    }

    #[rstest::rstest]
    #[case::before("before=")]
    #[case::since("since=")]
    #[case::label("label=")]
    fn given_an_empty_value_when_parsed_then_the_error_names_the_filter(#[case] spec: &str) {
        let error = Filters::parse(&[spec.to_string()], None)
            .err()
            .expect("refused");

        assert!(error.to_string().contains("a value is required"), "{error}");
    }

    #[test]
    fn given_before_naming_no_image_when_resolved_then_it_is_refused() {
        let mut filters = Filters::parse(&["before=ghost".to_string()], None).expect("parse");

        let error = filters.resolve(&[dated("a:1", 1)]).expect_err("refused");

        assert_eq!(error.to_string(), "No such image: ghost");
    }

    #[test]
    fn given_digests_when_rendered_then_the_digest_column_follows_the_tag() {
        let records = vec![record(&image("alpine:3", 1), false, Utc::now())];
        let template = Template::parse(DIGESTS_FORMAT, &FIELDS).expect("parse");

        let out = template.render(&records).unwrap();

        let header: Vec<&str> = out.lines().next().unwrap().split_whitespace().collect();
        assert_eq!(&header[..3], ["REPOSITORY", "TAG", "DIGEST"]);
    }
}
