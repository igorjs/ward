// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward container prune`, `image prune`, `volume prune` and `system prune`,
//! with Docker's confirmation prompt, `--filter` and report.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Result, bail};
use chrono::Utc;
use serde_json::json;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{
    ImageInfo, RemoveImageRequest, RemoveVolumeRequest, SandboxInfo, SandboxStatus,
};

use crate::client::{self, Resolved};
use crate::images::size_text;
use crate::lifecycle::remove_resolved;
use crate::prune_filter::Filter;

const BYTES_PER_MIB: u64 = 1024 * 1024;
const PROMPT: &str = "Are you sure you want to continue? [y/N] ";
const ANSWER_YES: &str = "y";
const WARN_CONTAINERS: &str = "all stopped containers";
const WARN_DANGLING_IMAGES: &str = "all dangling images";
const WARN_UNUSED_IMAGES: &str = "all images without at least one container associated to them";
const WARN_VOLUMES: &str = "all volumes not used by at least one container";

#[derive(clap::Args, Debug)]
pub struct ContainerArgs {
    /// Do not ask for confirmation.
    #[arg(short, long)]
    pub force: bool,
    /// Only sandboxes matching until=TIME, label=KEY[=VALUE] or
    /// label!=KEY[=VALUE]. Repeatable.
    #[arg(long)]
    pub filter: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct ImageArgs {
    /// Remove every image no sandbox uses, not only dangling ones. Ward has
    /// no dangling images, so without it nothing is removed.
    #[arg(short, long)]
    pub all: bool,
    /// Do not ask for confirmation.
    #[arg(short, long)]
    pub force: bool,
    /// Only images matching until=TIME, label=KEY[=VALUE], label!=KEY[=VALUE]
    /// or dangling=true|false. Repeatable.
    #[arg(long)]
    pub filter: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct VolumeArgs {
    /// Accepted for Docker compatibility. Every Ward volume is named and
    /// none can be attached to a sandbox yet, so all of them are pruned.
    #[arg(short, long)]
    pub all: bool,
    /// Do not ask for confirmation.
    #[arg(short, long)]
    pub force: bool,
    /// Only volumes matching until=TIME, label=KEY[=VALUE] or
    /// label!=KEY[=VALUE]. Repeatable.
    #[arg(long)]
    pub filter: Vec<String>,
}

#[derive(clap::Args, Debug)]
pub struct SystemArgs {
    /// Also remove every image no sandbox uses.
    #[arg(short, long)]
    pub all: bool,
    /// Also remove volumes.
    #[arg(long)]
    pub volumes: bool,
    /// Do not ask for confirmation.
    #[arg(short, long)]
    pub force: bool,
    /// Only objects matching until=TIME, label=KEY[=VALUE] or
    /// label!=KEY[=VALUE]. Repeatable.
    #[arg(long)]
    pub filter: Vec<String>,
}

/// What a prune removed.
#[derive(Debug, Default, PartialEq)]
struct Report {
    containers: Vec<String>,
    volumes: Vec<String>,
    images: Vec<String>,
    reclaimed: u64,
}

impl Report {
    fn text(&self) -> String {
        let mut out = String::new();
        for (title, lines) in [
            ("Deleted Containers:", &self.containers),
            ("Deleted Volumes:", &self.volumes),
            ("Deleted Images:", &self.images),
        ] {
            if !lines.is_empty() {
                out.push_str(title);
                out.push('\n');
                lines
                    .iter()
                    .for_each(|line| out.push_str(&format!("{line}\n")));
                out.push('\n');
            }
        }
        out.push_str(&format!(
            "Total reclaimed space: {}\n",
            size_text(self.reclaimed)
        ));
        out
    }

    /// Docker's API names for the same lists.
    fn json(&self) -> serde_json::Value {
        json!({
            "ContainersDeleted": self.containers,
            "VolumesDeleted": self.volumes,
            "ImagesDeleted": self.images,
            "SpaceReclaimed": self.reclaimed,
        })
    }

    fn print(&self, json: bool) {
        if json {
            println!("{}", self.json());
        } else {
            print!("{}", self.text());
        }
    }
}

/// Asks `warning` and reads the answer unless `force` is set. Only `y` goes
/// ahead, as in Docker. A closed input counts as no.
fn confirm(
    force: bool,
    warning: &str,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    if force {
        return Ok(());
    }
    write!(out, "{warning}\n{PROMPT}")?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case(ANSWER_YES) {
        return Ok(());
    }
    bail!("prune cancelled")
}

fn ask(force: bool, warning: &str) -> Result<()> {
    confirm(
        force,
        warning,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
}

fn created_seconds(info: &SandboxInfo) -> Option<i64> {
    info.created_at.map(|t| t.seconds)
}

/// Removes the stopped and exited sandboxes that pass `filter`. One the CLI
/// holds no token for is skipped with a note.
async fn prune_containers(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    filter: &Filter,
    report: &mut Report,
) -> Result<()> {
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let mut failed = 0;
    for info in &listed {
        let target = Resolved::from_info(token_store, info)?;
        let stopped = matches!(
            target.status,
            SandboxStatus::Stopped | SandboxStatus::Exited
        );
        if !stopped || !filter.allows(created_seconds(info), &info.labels) {
            continue;
        }
        if !target.has_token() {
            eprintln!("skipped: {}: no token in the local token store", target.id);
            continue;
        }
        match remove_resolved(c, token_store, &target).await {
            Ok(()) => report.containers.push(target.id),
            Err(e) => {
                failed += 1;
                eprintln!("error: {}: {e:#}", target.id);
            }
        }
    }
    if failed > 0 {
        bail!("{failed} sandboxes could not be removed");
    }
    Ok(())
}

/// The images no sandbox uses, by reference or by the digest it resolved to.
fn unused_images<'a>(images: &'a [ImageInfo], sandboxes: &[SandboxInfo]) -> Vec<&'a ImageInfo> {
    let references: HashSet<&str> = sandboxes.iter().map(|s| s.image.as_str()).collect();
    let digests: HashSet<&str> = sandboxes
        .iter()
        .map(|s| s.image_digest.as_str())
        .filter(|digest| !digest.is_empty())
        .collect();
    images
        .iter()
        .filter(|image| {
            !references.contains(image.reference.as_str())
                && !digests.contains(image.digest.as_str())
        })
        .collect()
}

/// Removes the unused images that pass `filter`. Without `all` only dangling
/// images qualify, and Ward has none.
async fn prune_images(
    c: &mut WardClient<Channel>,
    filter: &Filter,
    all: bool,
    report: &mut Report,
) -> Result<()> {
    if !all && !filter.wants_all_images() {
        return Ok(());
    }
    let images = c.list_images(()).await?.into_inner().images;
    let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let no_labels = HashMap::new();
    let mut counted = HashSet::new();
    for image in unused_images(&images, &sandboxes) {
        if !filter.allows(image.created_at.map(|t| t.seconds), &no_labels) {
            continue;
        }
        let request = RemoveImageRequest {
            reference: image.reference.clone(),
            force: false,
        };
        let reply = match c.remove_image(request).await {
            Ok(reply) => reply.into_inner(),
            Err(status) => {
                eprintln!("error: {}: {}", image.reference, status.message());
                continue;
            }
        };
        report
            .images
            .extend(reply.untagged.iter().map(|r| format!("untagged: {r}")));
        report
            .images
            .extend(reply.deleted.iter().map(|id| format!("deleted: {id}")));
        if !reply.deleted.is_empty() && counted.insert(image.digest.clone()) {
            report.reclaimed += image.size_bytes;
        }
    }
    Ok(())
}

/// Removes the volumes that pass `filter`. None is in use: sandboxes cannot
/// mount a volume yet.
async fn prune_volumes(
    c: &mut WardClient<Channel>,
    filter: &Filter,
    report: &mut Report,
) -> Result<()> {
    let volumes = c.list_volumes(()).await?.into_inner().volumes;
    let no_labels = HashMap::new();
    for volume in volumes {
        if !filter.allows(volume.created_at.map(|t| t.seconds), &no_labels) {
            continue;
        }
        match c
            .remove_volume(RemoveVolumeRequest {
                id: volume.id.clone(),
            })
            .await
        {
            Ok(_) => {
                report.volumes.push(volume.name);
                report.reclaimed += u64::from(volume.size_mb) * BYTES_PER_MIB;
            }
            Err(status) => eprintln!("error: {}: {}", volume.name, status.message()),
        }
    }
    Ok(())
}

pub async fn container(
    socket: &str,
    token_store: &Path,
    args: ContainerArgs,
    json: bool,
) -> Result<()> {
    let filter = Filter::parse(&args.filter, Utc::now(), false)?;
    ask(
        args.force,
        &format!("WARNING! This will remove {WARN_CONTAINERS}."),
    )?;
    let mut c = client::connect(socket).await?;
    let mut report = Report::default();
    prune_containers(&mut c, token_store, &filter, &mut report).await?;
    report.print(json);
    Ok(())
}

pub async fn image(socket: &str, args: ImageArgs, json: bool) -> Result<()> {
    let filter = Filter::parse(&args.filter, Utc::now(), true)?;
    let which = if args.all || filter.wants_all_images() {
        WARN_UNUSED_IMAGES
    } else {
        WARN_DANGLING_IMAGES
    };
    ask(args.force, &format!("WARNING! This will remove {which}."))?;
    let mut c = client::connect(socket).await?;
    let mut report = Report::default();
    prune_images(&mut c, &filter, args.all, &mut report).await?;
    report.print(json);
    Ok(())
}

pub async fn volume(socket: &str, args: VolumeArgs, json: bool) -> Result<()> {
    let filter = Filter::parse(&args.filter, Utc::now(), false)?;
    ask(
        args.force,
        &format!("WARNING! This will remove {WARN_VOLUMES}."),
    )?;
    let mut c = client::connect(socket).await?;
    let mut report = Report::default();
    prune_volumes(&mut c, &filter, &mut report).await?;
    report.print(json);
    Ok(())
}

pub async fn system(socket: &str, token_store: &Path, args: SystemArgs, json: bool) -> Result<()> {
    let filter = Filter::parse(&args.filter, Utc::now(), false)?;
    let images = if args.all {
        WARN_UNUSED_IMAGES
    } else {
        WARN_DANGLING_IMAGES
    };
    let mut warning = format!("WARNING! This will remove:\n  - {WARN_CONTAINERS}");
    if args.volumes {
        warning.push_str(&format!("\n  - {WARN_VOLUMES}"));
    }
    warning.push_str(&format!("\n  - {images}\n"));
    ask(args.force, &warning)?;
    let mut c = client::connect(socket).await?;
    let mut report = Report::default();
    prune_containers(&mut c, token_store, &filter, &mut report).await?;
    if args.volumes {
        prune_volumes(&mut c, &filter, &mut report).await?;
    }
    prune_images(&mut c, &filter, args.all, &mut report).await?;
    report.print(json);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn given_removed_objects_when_reported_then_sections_follow_docker() {
        let report = Report {
            containers: lines(&["abc"]),
            volumes: lines(&["data"]),
            images: lines(&["untagged: alpine:3", "deleted: sha256:1"]),
            reclaimed: 7_800_000,
        };

        assert_eq!(
            report.text(),
            "Deleted Containers:\nabc\n\nDeleted Volumes:\ndata\n\nDeleted Images:\nuntagged: alpine:3\ndeleted: sha256:1\n\nTotal reclaimed space: 7.8MB\n"
        );
    }

    #[test]
    fn given_nothing_removed_when_reported_then_only_the_total_prints() {
        assert_eq!(Report::default().text(), "Total reclaimed space: 0B\n");
    }

    #[test]
    fn given_a_report_when_json_then_docker_api_names_are_used() {
        let report = Report {
            containers: lines(&["abc"]),
            reclaimed: 5,
            ..Default::default()
        };

        let value = report.json();

        assert_eq!(value["ContainersDeleted"], json!(["abc"]));
        assert_eq!(value["SpaceReclaimed"], 5);
    }

    #[rstest::rstest]
    #[case::lower("y\n", true)]
    #[case::upper("Y\n", true)]
    #[case::no("n\n", false)]
    #[case::empty("\n", false)]
    #[case::closed("", false)]
    #[case::yes_word("yes\n", false)]
    fn given_an_answer_when_confirmed_then_only_y_goes_ahead(
        #[case] answer: &str,
        #[case] goes_ahead: bool,
    ) {
        let mut input = answer.as_bytes();
        let mut out = Vec::new();

        let result = confirm(false, "WARNING! This will remove x.", &mut input, &mut out);

        assert_eq!(result.is_ok(), goes_ahead);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            format!("WARNING! This will remove x.\n{PROMPT}")
        );
    }

    #[test]
    fn given_force_when_confirmed_then_nothing_is_asked() {
        let mut out = Vec::new();

        confirm(true, "warning", &mut std::io::empty(), &mut out).expect("forced");

        assert!(out.is_empty());
    }

    #[test]
    fn given_images_and_sandboxes_when_unused_found_then_references_and_digests_protect() {
        let image = |reference: &str, digest: &str| ImageInfo {
            reference: reference.into(),
            digest: digest.into(),
            ..Default::default()
        };
        let images = vec![
            image("alpine:3", "sha256:a"),
            image("alpine:latest", "sha256:a"),
            image("debian:12", "sha256:d"),
            image("busybox", "sha256:b"),
        ];
        let sandboxes = vec![
            SandboxInfo {
                image: "debian:12".into(),
                ..Default::default()
            },
            SandboxInfo {
                image: "elsewhere".into(),
                image_digest: "sha256:a".into(),
                ..Default::default()
            },
        ];

        let unused = unused_images(&images, &sandboxes);

        let names: Vec<&str> = unused.iter().map(|i| i.reference.as_str()).collect();
        assert_eq!(names, ["busybox"]);
    }
}
