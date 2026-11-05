// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward volume ls`, `inspect` and `rm`, with the options of the Docker
//! commands of the same names.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{RemoveVolumeRequest, SandboxInfo, VolumeInfo};

use crate::client;
use crate::images::{BYTES_PER_MIB, size_text};
use crate::inspect::rfc3339;
use crate::{output, template};

/// The only volume driver, and the only scope.
pub const DRIVER: &str = "local";
const SCOPE: &str = "local";
/// Volumes carry no labels, and `ls` shows no link count.
const NO_LABELS: &str = "";
const NO_LINKS: &str = "N/A";
/// The words `dangling=` accepts, as Docker does.
const TRUE_WORDS: [&str; 2] = ["true", "1"];
const FALSE_WORDS: [&str; 2] = ["false", "0"];
const ERROR_PREFIX: &str = "Error response from daemon: ";
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

/// A mount of the volume called `name`.
pub fn named_mount(name: &str, target: &str, readonly: bool) -> ward_core::pb::Mount {
    ward_core::pb::Mount {
        source: String::new(),
        target: target.to_string(),
        readonly,
        volume: name.to_string(),
    }
}

/// The mount for `SOURCE:TARGET`: a bind mount when `source` is an absolute
/// host path, otherwise the volume it names.
pub fn mount_for(source: &str, target: &str, readonly: bool) -> ward_core::pb::Mount {
    if source.starts_with('/') {
        ward_core::pb::Mount {
            source: source.to_string(),
            target: target.to_string(),
            readonly,
            volume: String::new(),
        }
    } else {
        named_mount(source, target, readonly)
    }
}

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
    /// A volume no sandbox mounts is dangling.
    dangling: Vec<bool>,
    drivers: Vec<String>,
    labels: Vec<String>,
}

/// Why a reference names no single volume.
#[derive(Debug, PartialEq)]
enum FindError {
    NotFound(String),
    Ambiguous(String),
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(reference) => write!(f, "get {reference}: no such volume"),
            Self::Ambiguous(reference) => write!(f, "multiple volumes match '{reference}'"),
        }
    }
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
                    filters.dangling.push(if TRUE_WORDS.contains(&value) {
                        true
                    } else if FALSE_WORDS.contains(&value) {
                        false
                    } else {
                        bail!("invalid filter 'dangling={value}'")
                    });
                }
                FILTER_DRIVER => filters.drivers.push(value.to_string()),
                FILTER_LABEL => filters.labels.push(value.to_string()),
                other => bail!("invalid filter '{other}'"),
            }
        }
        Ok(filters)
    }

    /// Volumes carry no labels, so any `label=` filter matches none. Values
    /// of one key are alternatives, as in Docker. `links` is how many
    /// sandboxes mount the volume.
    fn allows(&self, volume: &VolumeInfo, links: usize) -> bool {
        self.labels.is_empty()
            && (self.dangling.is_empty() || self.dangling.contains(&(links == 0)))
            && (self.drivers.is_empty() || self.drivers.iter().any(|d| d == DRIVER))
            && (self.names.is_empty() || self.names.iter().any(|n| volume.name.contains(n)))
    }
}

/// The volume `reference` names: a name, an ID or a unique ID prefix.
fn find<'a>(volumes: &'a [VolumeInfo], reference: &str) -> Result<&'a VolumeInfo, FindError> {
    if let Some(volume) = volumes
        .iter()
        .find(|v| v.name == reference || v.id == reference)
    {
        return Ok(volume);
    }
    let mut prefixed = volumes.iter().filter(|v| v.id.starts_with(reference));
    match (prefixed.next(), prefixed.next()) {
        (Some(volume), None) if !reference.is_empty() => Ok(volume),
        (Some(_), Some(_)) if !reference.is_empty() => {
            Err(FindError::Ambiguous(reference.to_string()))
        }
        _ => Err(FindError::NotFound(reference.to_string())),
    }
}

fn created_seconds(volume: &VolumeInfo) -> Option<i64> {
    volume.created_at.map(|t| t.seconds)
}

/// The `volume inspect` object, in Docker's shape plus Ward's `Id` and `SizeMB`.
fn inspect_object(volume: &VolumeInfo) -> Map<String, Value> {
    Map::from_iter([
        (
            "CreatedAt".to_string(),
            json!(created_seconds(volume).map(rfc3339)),
        ),
        ("Driver".to_string(), json!(DRIVER)),
        ("Labels".to_string(), json!({})),
        ("Mountpoint".to_string(), json!(volume.mount_path)),
        ("Name".to_string(), json!(volume.name)),
        ("Options".to_string(), Value::Null),
        ("Scope".to_string(), json!(SCOPE)),
        ("Id".to_string(), json!(volume.id)),
        ("SizeMB".to_string(), json!(volume.size_mb)),
    ])
}

/// The `ls --format` record.
fn list_record(volume: &VolumeInfo) -> Map<String, Value> {
    let size = u64::from(volume.size_mb) * BYTES_PER_MIB;
    Map::from_iter([
        ("Driver".to_string(), json!(DRIVER)),
        ("Name".to_string(), json!(volume.name)),
        ("ID".to_string(), json!(volume.id)),
        ("Mountpoint".to_string(), json!(volume.mount_path)),
        ("Scope".to_string(), json!(SCOPE)),
        ("Size".to_string(), json!(size_text(size))),
        ("Labels".to_string(), json!(NO_LABELS)),
        ("Links".to_string(), json!(NO_LINKS)),
    ])
}

/// How many sandboxes, stopped ones included, mount each volume by name.
pub fn link_counts(sandboxes: &[SandboxInfo]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for sandbox in sandboxes {
        let names: HashSet<&str> = sandbox
            .mounts
            .iter()
            .filter(|m| !m.volume.is_empty())
            .map(|m| m.volume.as_str())
            .collect();
        for name in names {
            *counts.entry(name.to_string()).or_default() += 1;
        }
    }
    counts
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
    let links = link_counts(&c.list_sandboxes(()).await?.into_inner().sandboxes);
    let volumes: Vec<VolumeInfo> = all_volumes(&mut c)
        .await?
        .into_iter()
        .filter(|v| filters.allows(v, links.get(&v.name).copied().unwrap_or_default()))
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

/// Prints the volumes found. Returns false when any reference named none, after
/// printing each such error, so the caller can exit non-zero.
///
/// # Errors
///
/// Fails on a bad template or when the daemon cannot be reached.
pub async fn inspect(socket: &str, args: InspectArgs) -> Result<bool> {
    let parsed = args
        .format
        .as_deref()
        .map(|format| template::Template::parse(format, &[(template::ANY_FIELD, "")]))
        .transpose()?;
    let mut c = client::connect(socket).await?;
    let volumes = all_volumes(&mut c).await?;
    let mut found = Vec::new();
    let mut all_found = true;
    for reference in &args.volumes {
        match find(&volumes, reference) {
            Ok(volume) => found.push(inspect_object(volume)),
            Err(e) => {
                all_found = false;
                eprintln!("{ERROR_PREFIX}{e}");
            }
        }
    }
    match parsed {
        Some(template) => print!("{}", template.render(&found)?),
        None => println!("{}", serde_json::to_string_pretty(&found)?),
    }
    Ok(all_found)
}

/// Removes each volume in turn and keeps going after a failure. Returns false
/// when any removal failed, after printing each error. With `-f` a volume that
/// does not exist is skipped.
///
/// # Errors
///
/// Fails when the daemon cannot be reached.
pub async fn remove(socket: &str, args: RemoveArgs, json: bool) -> Result<bool> {
    let mut c = client::connect(socket).await?;
    let mut volumes = all_volumes(&mut c).await?;
    let mut all_removed = true;
    for reference in &args.volumes {
        let id = match find(&volumes, reference) {
            Ok(volume) => volume.id.clone(),
            Err(FindError::NotFound(_)) if args.force => continue,
            Err(e) => {
                all_removed = false;
                eprintln!("{ERROR_PREFIX}{e}");
                continue;
            }
        };
        let request = RemoveVolumeRequest { id: id.clone() };
        match c.remove_volume(request).await {
            Ok(_) => {
                // A repeat of the same volume must now count as missing.
                volumes.retain(|v| v.id != id);
                if json {
                    println!("{}", json!({"removed": reference}));
                } else {
                    println!("{reference}");
                }
            }
            Err(status) => {
                all_removed = false;
                eprintln!("{ERROR_PREFIX}{}", status.message());
            }
        }
    }
    Ok(all_removed)
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
    #[case::unknown("nope", FindError::NotFound("nope".into()), "get nope: no such volume")]
    #[case::empty("", FindError::NotFound(String::new()), "get : no such volume")]
    #[case::ambiguous("aa", FindError::Ambiguous("aa".into()), "multiple volumes match 'aa'")]
    fn given_a_bad_reference_when_found_then_the_error_is_typed_and_worded_like_docker(
        #[case] reference: &str,
        #[case] expected: FindError,
        #[case] text: &str,
    ) {
        let volumes = fleet();

        let error = find(&volumes, reference).expect_err("no single volume");

        assert_eq!(error, expected);
        assert_eq!(error.to_string(), text);
    }

    #[rstest::rstest]
    #[case::by_name(&["name=ca"], &["cache"])]
    #[case::dangling(&["dangling=true"], &["cache", "data"])]
    #[case::not_dangling(&["dangling=false"], &[])]
    #[case::driver(&["driver=local"], &["cache", "data"])]
    #[case::other_driver(&["driver=nfs"], &[])]
    #[case::label_matches_none(&["label=env"], &[])]
    #[case::repeated_driver_is_an_alternative(&["driver=nfs", "driver=local"], &["cache", "data"])]
    #[case::repeated_dangling_is_an_alternative(&["dangling=false", "dangling=true"], &["cache", "data"])]
    #[case::one_of_two_names(&["name=ca", "name=zzz"], &["cache"])]
    #[case::numeric_dangling(&["dangling=1"], &["cache", "data"])]
    #[case::numeric_not_dangling(&["dangling=0"], &[])]
    fn given_filters_when_applied_then_the_matching_volumes_remain(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
        let filters = Filters::parse(&specs).expect("parse");

        let kept: Vec<String> = fleet()
            .into_iter()
            .filter(|v| filters.allows(v, 0))
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
