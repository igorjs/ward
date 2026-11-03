// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward history IMAGE`: the layers of an image, newest first.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use ward_core::pb::{HistoryLayer, ImageHistoryRequest};

use crate::client;
use crate::images::{BYTES_PER_UNIT, CREATED_AT_FORMAT, DIGEST_PREFIX, SHORT_ID_LEN, SIZE_UNITS};
use crate::inspect::{duration_text, seconds_since};
use crate::stats::human_size;
use crate::template::Template;

const DEFAULT_FORMAT: &str =
    "table {{.ID}}\t{{.CreatedSince}}\t{{.CreatedBy}}\t{{.Size}}\t{{.Comment}}";
/// Docker shows this in the IMAGE column of every layer but the top one.
const MISSING_ID: &str = "<missing>";
/// Shown when the image config gave no creation time.
const NO_TIME: &str = "N/A";
const CREATED_BY_WIDTH: usize = 45;
const ELLIPSIS: &str = "...";
const FIELDS: [(&str, &str); 6] = [
    ("ID", "IMAGE"),
    ("CreatedSince", "CREATED"),
    ("CreatedAt", "CREATED AT"),
    ("CreatedBy", "CREATED BY"),
    ("Size", "SIZE"),
    ("Comment", "COMMENT"),
];

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Print only image IDs.
    #[arg(short, long)]
    pub quiet: bool,
    /// Do not cut the CREATED BY column or the image ID.
    #[arg(long)]
    pub no_trunc: bool,
    /// Go template for each layer: .ID .CreatedSince .CreatedAt .CreatedBy
    /// .Size .Comment, the `table ` prefix, and `json`.
    #[arg(long)]
    pub format: Option<String>,
    /// Tag, image ID or unique ID prefix.
    pub image: String,
}

pub async fn run(socket: &str, args: Args, json: bool) -> Result<()> {
    let template = Template::parse(args.format.as_deref().unwrap_or(DEFAULT_FORMAT), &FIELDS)?;
    let mut c = client::connect(socket).await?;
    let reply = c
        .image_history(ImageHistoryRequest {
            reference: args.image,
        })
        .await?
        .into_inner();
    let now = Utc::now();
    let records: Vec<Map<String, Value>> = reply
        .layers
        .iter()
        .enumerate()
        .map(|(position, layer)| {
            let id = match position {
                0 => image_id(&reply.image_id, args.no_trunc),
                _ => MISSING_ID.to_string(),
            };
            record(layer, id, args.no_trunc, now)
        })
        .collect();
    if args.quiet {
        for record in &records {
            println!("{}", record["ID"].as_str().unwrap_or_default());
        }
    } else if json && args.format.is_none() {
        println!(
            "{}",
            Value::Array(records.into_iter().map(Value::Object).collect())
        );
    } else {
        print!("{}", template.render(&records));
    }
    Ok(())
}

fn image_id(digest: &str, no_trunc: bool) -> String {
    if no_trunc {
        return digest.to_string();
    }
    digest
        .strip_prefix(DIGEST_PREFIX)
        .unwrap_or(digest)
        .chars()
        .take(SHORT_ID_LEN)
        .collect()
}

/// A command cut to Docker's column width on one line.
fn created_by(command: &str, no_trunc: bool) -> String {
    let flat = command.replace('\t', " ");
    if no_trunc || flat.chars().count() <= CREATED_BY_WIDTH {
        return flat;
    }
    let keep = CREATED_BY_WIDTH - ELLIPSIS.len();
    format!("{}{ELLIPSIS}", flat.chars().take(keep).collect::<String>())
}

fn record(
    layer: &HistoryLayer,
    id: String,
    no_trunc: bool,
    now: DateTime<Utc>,
) -> Map<String, Value> {
    let created_at = layer
        .created
        .as_ref()
        .and_then(|t| DateTime::from_timestamp(t.seconds, 0))
        .map_or_else(
            || NO_TIME.to_string(),
            |t| t.format(CREATED_AT_FORMAT).to_string(),
        );
    let since = seconds_since(&layer.created, now.timestamp()).map_or_else(
        || NO_TIME.to_string(),
        |s| format!("{} ago", duration_text(s)),
    );
    match json!({
        "ID": id,
        "CreatedSince": since,
        "CreatedAt": created_at,
        "CreatedBy": created_by(&layer.created_by, no_trunc),
        "Size": human_size(layer.size_bytes, BYTES_PER_UNIT, &SIZE_UNITS),
        "Comment": layer.comment,
    }) {
        Value::Object(map) => map,
        _ => unreachable!("json! of an object literal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_long_command_when_cut_then_it_ends_with_an_ellipsis_at_the_docker_width() {
        // Arrange
        let command = "x".repeat(60);

        // Act
        let cut = created_by(&command, false);

        // Assert
        assert_eq!(cut.chars().count(), CREATED_BY_WIDTH);
        assert!(cut.ends_with(ELLIPSIS));
        assert_eq!(created_by(&command, true), command);
    }

    #[test]
    fn given_layers_when_rendered_then_only_the_top_one_shows_the_image_id() {
        // Arrange
        let layer = HistoryLayer {
            created: Some(prost_types::Timestamp::default()),
            created_by: "RUN make".to_string(),
            size_bytes: 7_800_000,
            comment: String::new(),
        };
        let digest = format!("{DIGEST_PREFIX}{}", "ab".repeat(32));
        let records = vec![
            record(&layer, image_id(&digest, false), false, Utc::now()),
            record(&layer, MISSING_ID.to_string(), false, Utc::now()),
        ];
        let template = Template::parse(DEFAULT_FORMAT, &FIELDS).expect("parse");

        // Act
        let out = template.render(&records);

        // Assert
        let rows: Vec<&str> = out.lines().skip(1).collect();
        assert!(rows[0].starts_with("abababababab"), "{}", rows[0]);
        assert!(rows[1].starts_with(MISSING_ID), "{}", rows[1]);
        assert!(rows[0].contains("7.8MB"), "{}", rows[0]);
    }
}
