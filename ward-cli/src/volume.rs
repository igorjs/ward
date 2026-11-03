// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward volume ls`, `inspect` and `rm`, with the options of the Docker
//! commands of the same names.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{RemoveVolumeRequest, VolumeInfo};

use crate::client::{self, PlainError};
use crate::images::size_text;
use crate::inspect::rfc3339;
use crate::{output, template};

const BYTES_PER_MIB: u64 = 1024 * 1024;
/// The only volume driver, and the only scope.
const DRIVER: &str = "local";
const SCOPE: &str = "local";
const FILTER_NAME: &str = "name";
const FILTER_DANGLING: &str = "dangling";
const FILTER_DRIVER: &str = "driver";
const FILTER_LABEL: &str = "label";
const LIST_HEADERS: [&str; 3] = ["id", "name", "size_mb"];
/// Each `ls --format` field with the header a `table` template gives it.
const FIELDS: [(&str, &str); 8] = [
    ("Driver", "DRIVER"),
    ("Name", "VOLUME NAME"),
    ("ID", "ID"),
    ("Mountpoint", "MOUNTPOINT"),
    ("Scope", "SCOPE"),
    ("Size", "SIZE"),
    ("Labels", "LABELS"),
    ("Links", "LINKS"),
];

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Print only the volume names.
    #[arg(short, long)]
    pub quiet: bool,
    /// Go template for each volume, or `json`. Fields: .Name .ID .Driver
    /// .Scope .Mountpoint .Size .Labels .Links.
    #[arg(long)]
    pub format: Option<String>,
    /// Only volumes matching name= (a substring), dangling=true|false,
    /// driver=local or label=. Repeatable.
    #[arg(short, long)]
    pub filter: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct InspectArgs {
    /// Go template for each volume.
    #[arg(short, long)]
    pub format: Option<String>,
    /// Volume names, IDs or ID prefixes.
    #[arg(required = true)]
    pub volumes: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct RemoveArgs {
    /// Do not fail for a volume that does not exist.
    #[arg(short, long)]
    pub force: bool,
    /// Volume names, IDs or ID prefixes.
    #[arg(required = true)]
    pub volumes: Vec<String>,
}

/// Parsed `ls --filter` values. A volume must pass every one.
#[derive(Debug, Default, PartialEq)]
struct Filters {
    names: Vec<String>,
    /// Ward volumes cannot be attached to a sandbox, so all are dangling.
    dangling: Option<bool>,
    driver: Option<String>,
    labels: Vec<String>,
}

impl Filters {
    fn parse(specs: &[String]) -> Result<Self> {
        let mut filters = Self::default();
        for spec in specs {
            let Some((key, value)) = spec.split_once('=') else {
                bail!("bad format of filter (expected name=value): {spec}");
            };
            match key {
                FILTER_NAME => filters.names.push(value.to_string()),
                FILTER_DANGLING => {
                    filters.dangling = Some(match value {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => bail!("invalid filter 'dangling={value}'"),
                    });
                }
                FILTER_DRIVER => filters.driver = Some(value.to_string()),
                FILTER_LABEL => filters.labels.push(value.to_string()),
                other => bail!("invalid filter '{other}'"),
            }
        }
        Ok(filters)
    }

    /// Volumes carry no labels, so any `label=` filter matches none.
    fn allows(&self, volume: &VolumeInfo) -> bool {
        self.labels.is_empty()
            && self.dangling != Some(false)
            && self.driver.as_deref().is_none_or(|d| d == DRIVER)
            && (self.names.is_empty() || self.names.iter().any(|n| volume.name.contains(n)))
    }
}

/// The volume `reference` names: a name, an ID or a unique ID prefix.
fn find<'a>(volumes: &'a [VolumeInfo], reference: &str) -> Result<&'a VolumeInfo> {
    if let Some(volume) = volumes
        .iter()
        .find(|v| v.name == reference || v.id == reference)
    {
        return Ok(volume);
    }
    let mut prefixed = volumes.iter().filter(|v| v.id.starts_with(reference));
    match (prefixed.next(), prefixed.next()) {
        (Some(volume), None) if !reference.is_empty() => Ok(volume),
        (Some(_), Some(_)) => bail!("multiple volumes match '{reference}'"),
        _ => bail!("get {reference}: no such volume"),
    }
}

fn created_seconds(volume: &VolumeInfo) -> Option<i64> {
    volume.created_at.map(|t| t.seconds)
}

/// The `volume inspect` object, in Docker's shape plus Ward's `Id` and `SizeMB`.
fn inspect_object(volume: &VolumeInfo) -> Map<String, Value> {
    let Value::Object(map) = json!({
        "CreatedAt": created_seconds(volume).map(rfc3339),
        "Driver": DRIVER,
        "Labels": {},
        "Mountpoint": volume.mount_path,
        "Name": volume.name,
        "Options": null,
        "Scope": SCOPE,
        "Id": volume.id,
        "SizeMB": volume.size_mb,
    }) else {
        unreachable!("json! of an object")
    };
    map
}

/// The `ls --format` record.
fn list_record(volume: &VolumeInfo) -> Map<String, Value> {
    let size = u64::from(volume.size_mb) * BYTES_PER_MIB;
    let Value::Object(map) = json!({
        "Driver": DRIVER,
        "Name": volume.name,
        "ID": volume.id,
        "Mountpoint": volume.mount_path,
        "Scope": SCOPE,
        "Size": size_text(size),
        "Labels": "",
        "Links": "N/A",
    }) else {
        unreachable!("json! of an object")
    };
    map
}

async fn all_volumes(c: &mut WardClient<Channel>) -> Result<Vec<VolumeInfo>> {
    Ok(c.list_volumes(()).await?.into_inner().volumes)
}

pub async fn list(socket: &str, args: ListArgs, json: bool, no_pretty: bool) -> Result<()> {
    let filters = Filters::parse(&args.filter)?;
    let parsed = args
        .format
        .as_deref()
        .map(|format| template::Template::parse(format, &FIELDS))
        .transpose()?;
    let mut c = client::connect(socket).await?;
    let volumes: Vec<VolumeInfo> = all_volumes(&mut c)
        .await?
        .into_iter()
        .filter(|v| filters.allows(v))
        .collect();
    if args.quiet {
        volumes.iter().for_each(|v| println!("{}", v.name));
    } else if let Some(template) = parsed {
        let records: Vec<_> = volumes.iter().map(list_record).collect();
        print!("{}", template.render(&records)?);
    } else {
        let rows: Vec<Vec<Value>> = volumes
            .iter()
            .map(|v| vec![v.id.clone().into(), v.name.clone().into(), v.size_mb.into()])
            .collect();
        output::emit_rows(json, no_pretty, &LIST_HEADERS, &rows)?;
    }
    Ok(())
}

pub async fn inspect(socket: &str, args: InspectArgs) -> Result<()> {
    let parsed = args
        .format
        .as_deref()
        .map(|format| template::Template::parse(format, &[(template::ANY_FIELD, "")]))
        .transpose()?;
    let mut c = client::connect(socket).await?;
    let volumes = all_volumes(&mut c).await?;
    let mut found = Vec::new();
    let mut missing = 0;
    for reference in &args.volumes {
        match find(&volumes, reference) {
            Ok(volume) => found.push(inspect_object(volume)),
            Err(e) => {
                missing += 1;
                eprintln!("Error: {e}");
            }
        }
    }
    match parsed {
        Some(template) => print!("{}", template.render(&found)?),
        None => println!("{}", serde_json::to_string_pretty(&found)?),
    }
    if missing > 0 {
        return Err(anyhow::Error::new(PlainError(format!(
            "Error: no such volume ({missing} of {})",
            args.volumes.len()
        ))));
    }
    Ok(())
}

pub async fn remove(socket: &str, args: RemoveArgs, json: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let volumes = all_volumes(&mut c).await?;
    let mut failed = 0;
    for reference in &args.volumes {
        let result = match find(&volumes, reference) {
            Ok(volume) => c
                .remove_volume(RemoveVolumeRequest {
                    id: volume.id.clone(),
                })
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
            Err(e) if args.force && e.to_string().contains("no such volume") => continue,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) if json => println!("{}", json!({"removed": reference})),
            Ok(()) => println!("{reference}"),
            Err(e) => {
                failed += 1;
                eprintln!("Error response from daemon: {e}");
            }
        }
    }
    if failed > 0 {
        return Err(anyhow::Error::new(PlainError(format!(
            "Error: failed to remove {failed} of {} volumes",
            args.volumes.len()
        ))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(id: &str, name: &str) -> VolumeInfo {
        VolumeInfo {
            id: id.into(),
            name: name.into(),
            size_mb: 100,
            mount_path: format!("/data/{name}"),
            created_at: Some(prost_types::Timestamp {
                seconds: 0,
                nanos: 0,
            }),
        }
    }

    fn fleet() -> Vec<VolumeInfo> {
        vec![volume("aaa111", "cache"), volume("aab222", "data")]
    }

    #[rstest::rstest]
    #[case::name("data", Some("data"))]
    #[case::id("aaa111", Some("cache"))]
    #[case::unique_prefix("aab", Some("data"))]
    #[case::ambiguous_prefix("aa", None)]
    #[case::unknown("nope", None)]
    #[case::empty("", None)]
    fn given_a_reference_when_found_then_name_id_and_unique_prefix_resolve(
        #[case] reference: &str,
        #[case] expected: Option<&str>,
    ) {
        let volumes = fleet();

        let found = find(&volumes, reference).ok().map(|v| v.name.as_str());

        assert_eq!(found, expected);
    }

    #[rstest::rstest]
    #[case::by_name(&["name=ca"], &["cache"])]
    #[case::dangling(&["dangling=true"], &["cache", "data"])]
    #[case::not_dangling(&["dangling=false"], &[])]
    #[case::driver(&["driver=local"], &["cache", "data"])]
    #[case::other_driver(&["driver=nfs"], &[])]
    #[case::label_matches_none(&["label=env"], &[])]
    fn given_filters_when_applied_then_the_matching_volumes_remain(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
        let filters = Filters::parse(&specs).expect("parse");

        let kept: Vec<String> = fleet()
            .into_iter()
            .filter(|v| filters.allows(v))
            .map(|v| v.name)
            .collect();

        assert_eq!(kept, expected);
    }

    #[rstest::rstest]
    #[case::no_equals("name")]
    #[case::unknown_key("colour=red")]
    #[case::bad_dangling("dangling=maybe")]
    fn given_a_bad_filter_when_parsed_then_it_is_refused(#[case] spec: &str) {
        assert!(Filters::parse(&[spec.to_string()]).is_err());
    }

    #[test]
    fn given_a_volume_when_inspected_then_docker_fields_come_with_wards_id() {
        let object = inspect_object(&volume("aaa111", "cache"));

        assert_eq!(object["Name"], "cache");
        assert_eq!(object["Driver"], "local");
        assert_eq!(object["Mountpoint"], "/data/cache");
        assert_eq!(object["CreatedAt"], "1970-01-01T00:00:00Z");
        assert_eq!(object["Id"], "aaa111");
        assert_eq!(object["Labels"], json!({}));
    }

    #[test]
    fn given_a_volume_when_listed_by_template_then_size_is_human_readable() {
        let template =
            template::Template::parse("{{.Name}} {{.Size}} {{.Driver}}", &FIELDS).expect("parse");

        let out = template
            .render(&[list_record(&volume("aaa111", "cache"))])
            .expect("render");

        assert_eq!(out, "cache 104.9MB local\n");
    }
}
