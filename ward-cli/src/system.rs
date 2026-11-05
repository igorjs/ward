// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward version`, `ward system info` and `ward system df`.

use anyhow::Result;
use serde_json::{Value, json};
use ward_core::pb::SandboxStatus;

use crate::images::{BYTES_PER_MIB, size_text};
use crate::ps_columns::uses_image;
use crate::template::{ANY_FIELD, Template};
use crate::{client, output};

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const INDENT: &str = " ";
/// Shown for a figure Ward does not measure, as `ward stats` does.
const UNMEASURED: &str = "--";
const DF_HEADERS: [&str; 5] = ["TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"];
const DF_DEFAULT_FORMAT: &str =
    "table {{.Type}}\t{{.TotalCount}}\t{{.Active}}\t{{.Size}}\t{{.Reclaimable}}";
/// Each `system df --format` field with the header a `table` template gives it.
const DF_FIELDS: [(&str, &str); 5] = [
    ("Type", "TYPE"),
    ("TotalCount", "TOTAL"),
    ("Active", "ACTIVE"),
    ("Size", "SIZE"),
    ("Reclaimable", "RECLAIMABLE"),
];
const PERCENT: u64 = 100;
const KIND_IMAGES: &str = "Images";
const KIND_CONTAINERS: &str = "Containers";
const KIND_VOLUMES: &str = "Local Volumes";
const KIND_BUILD_CACHE: &str = "Build Cache";
/// How `info` names the isolation, in place of Docker's storage driver.
const ISOLATION: &str = "microVM";

/// The `docker version` record: `.Client.Version`, `.Server.Version`, and so on.
fn version_record(server: Option<&ward_core::pb::DaemonInfo>) -> Value {
    json!({
        "Client": {"Version": CLIENT_VERSION},
        "Server": server.map(|info| json!({
            "Version": info.version,
            "Os": info.platform,
            "Arch": info.arch,
            "Backend": info.backend,
        })),
    })
}

fn version_text(record: &Value) -> String {
    let field = |section: &str, key: &str| record[section][key].as_str().unwrap_or_default();
    let mut text = format!(
        "Client:\n{INDENT}Version:  {}\n",
        field("Client", "Version")
    );
    if record["Server"].is_object() {
        text.push_str(&format!(
            "\nServer:\n{INDENT}Version:  {}\n{INDENT}OS/Arch:  {}/{}\n{INDENT}Backend:  {}\n",
            field("Server", "Version"),
            field("Server", "Os"),
            field("Server", "Arch"),
            field("Server", "Backend"),
        ));
    }
    text
}

/// Prints the client and daemon versions. The client half prints even when the
/// daemon is unreachable, and the command then fails, like `docker version`.
pub async fn version(socket: &str, format: Option<&str>, json: bool) -> Result<()> {
    let template = format
        .map(|format| Template::parse(format, &[(ANY_FIELD, "")]))
        .transpose()?;
    let server = async {
        let mut c = client::connect(socket).await?;
        anyhow::Ok(c.get_info(()).await?.into_inner())
    }
    .await;
    let record = version_record(server.as_ref().ok());
    match (template, record) {
        (Some(template), Value::Object(map)) => print!("{}", template.render(&[map])?),
        (_, record) if json => println!("{record}"),
        (_, record) => print!("{}", version_text(&record)),
    }
    server.map(|_| ())
}

/// Prints daemon information.
pub async fn info(socket: &str, format: Option<&str>, json: bool) -> Result<()> {
    let template = format
        .map(|format| Template::parse(format, &[(ANY_FIELD, "")]))
        .transpose()?;
    let mut c = client::connect(socket).await?;
    let resp = c.get_info(()).await?.into_inner();
    let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let images = c.list_images(()).await?.into_inner().images;
    let record = info_record(&resp, &sandboxes, images.len());
    if let Some(template) = template {
        print!("{}", template.render(&[record])?);
    } else if json {
        println!(
            "{}",
            json!({
                "version": resp.version,
                "platform": resp.platform,
                "arch": resp.arch,
                "backend": resp.backend,
            })
        );
    } else {
        print!("{}", info_text(&record));
    }
    Ok(())
}

/// The `docker info` record with the fields Ward has, under Docker's names.
fn info_record(
    info: &ward_core::pb::DaemonInfo,
    sandboxes: &[ward_core::pb::SandboxInfo],
    images: usize,
) -> serde_json::Map<String, Value> {
    let alive = |s: &&ward_core::pb::SandboxInfo| s.status == SandboxStatus::Running as i32;
    let paused = sandboxes.iter().filter(alive).filter(|s| s.paused).count();
    let running = sandboxes.iter().filter(alive).count() - paused;
    let Value::Object(map) = json!({
        "ID": info.id,
        "Name": info.hostname,
        "ServerVersion": info.version,
        "OSType": info.platform,
        "OperatingSystem": info.operating_system,
        "KernelVersion": info.kernel_version,
        "Architecture": info.arch,
        "Driver": info.backend,
        "NCPU": info.ncpu,
        "MemTotal": info.mem_total_bytes,
        "DockerRootDir": info.root_dir,
        "Containers": sandboxes.len(),
        "ContainersRunning": running,
        "ContainersPaused": paused,
        "ContainersStopped": sandboxes.len() - running - paused,
        "Images": images,
    }) else {
        unreachable!("json! of an object")
    };
    map
}

/// The sectioned text of `docker info`.
fn info_text(record: &serde_json::Map<String, Value>) -> String {
    let text = |key: &str| match record.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let memory = size_text(record.get("MemTotal").and_then(Value::as_u64).unwrap_or(0));
    format!(
        "Server:\n\
         {INDENT}Containers: {}\n\
         {INDENT}{INDENT}Running: {}\n\
         {INDENT}{INDENT}Paused: {}\n\
         {INDENT}{INDENT}Stopped: {}\n\
         {INDENT}Images: {}\n\
         {INDENT}Server Version: {}\n\
         {INDENT}Isolation: {ISOLATION} ({})\n\
         {INDENT}Kernel Version: {}\n\
         {INDENT}Operating System: {}\n\
         {INDENT}OSType: {}\n\
         {INDENT}Architecture: {}\n\
         {INDENT}CPUs: {}\n\
         {INDENT}Total Memory: {memory}\n\
         {INDENT}Name: {}\n\
         {INDENT}ID: {}\n\
         {INDENT}Docker Root Dir: {}\n",
        text("Containers"),
        text("ContainersRunning"),
        text("ContainersPaused"),
        text("ContainersStopped"),
        text("Images"),
        text("ServerVersion"),
        text("Driver"),
        text("KernelVersion"),
        text("OperatingSystem"),
        text("OSType"),
        text("Architecture"),
        text("NCPU"),
        text("Name"),
        text("ID"),
        text("DockerRootDir"),
    )
}

/// One `docker system df` row: the figures of one kind of object.
struct DfRow {
    kind: &'static str,
    total: usize,
    active: usize,
    size: String,
    reclaimable: String,
}

impl DfRow {
    fn cells(&self) -> Vec<Value> {
        vec![
            self.kind.into(),
            self.total.into(),
            self.active.into(),
            self.size.clone().into(),
            self.reclaimable.clone().into(),
        ]
    }

    fn record(&self) -> serde_json::Map<String, Value> {
        let Value::Object(map) = json!({
            "Type": self.kind,
            "TotalCount": self.total,
            "Active": self.active,
            "Size": self.size,
            "Reclaimable": self.reclaimable,
        }) else {
            unreachable!("json! of an object")
        };
        map
    }
}

/// `1.2GB (66%)`: the reclaimable bytes and their share of the total.
fn reclaimable_text(reclaimable: u64, total: u64) -> String {
    if total == 0 {
        return size_text(0);
    }
    let percent = reclaimable
        .saturating_mul(PERCENT)
        .checked_div(total)
        .unwrap_or(0);
    format!("{} ({percent}%)", size_text(reclaimable))
}

/// The rows of `docker system df`, from what the daemon holds.
fn df_rows(
    sandboxes: &[ward_core::pb::SandboxInfo],
    images: &[ward_core::pb::ImageInfo],
    volumes: &[ward_core::pb::VolumeInfo],
) -> Vec<DfRow> {
    // A paused sandbox is not counted as running, as in `info`.
    let running = sandboxes
        .iter()
        .filter(|s| s.status == SandboxStatus::Running as i32 && !s.paused)
        .count();
    let used = |image: &&ward_core::pb::ImageInfo| sandboxes.iter().any(|s| uses_image(s, image));
    let image_bytes: u64 = images.iter().map(|i| i.size_bytes).sum();
    let unused_bytes: u64 = images
        .iter()
        .filter(|i| !used(i))
        .map(|i| i.size_bytes)
        .sum();
    // A volume cannot be attached to a sandbox yet, so none is in use.
    let volume_bytes: u64 = volumes
        .iter()
        .map(|v| u64::from(v.size_mb) * BYTES_PER_MIB)
        .sum();
    vec![
        DfRow {
            kind: KIND_IMAGES,
            total: images.len(),
            active: images.iter().filter(used).count(),
            size: size_text(image_bytes),
            reclaimable: reclaimable_text(unused_bytes, image_bytes),
        },
        DfRow {
            kind: KIND_CONTAINERS,
            total: sandboxes.len(),
            active: running,
            size: UNMEASURED.into(),
            reclaimable: UNMEASURED.into(),
        },
        DfRow {
            kind: KIND_VOLUMES,
            total: volumes.len(),
            active: 0,
            size: size_text(volume_bytes),
            reclaimable: reclaimable_text(volume_bytes, volume_bytes),
        },
        DfRow {
            kind: KIND_BUILD_CACHE,
            total: 0,
            active: 0,
            size: size_text(0),
            reclaimable: size_text(0),
        },
    ]
}

/// Prints the usage table of `docker system df`.
pub async fn df(socket: &str, format: Option<&str>, json: bool, no_pretty: bool) -> Result<()> {
    let template = Template::parse(format.unwrap_or(DF_DEFAULT_FORMAT), &DF_FIELDS)?;
    let mut c = client::connect(socket).await?;
    let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let images = c.list_images(()).await?.into_inner().images;
    let volumes = c.list_volumes(()).await?.into_inner().volumes;
    let rows = df_rows(&sandboxes, &images, &volumes);
    if format.is_some() || !(json || no_pretty) {
        let records: Vec<_> = rows.iter().map(DfRow::record).collect();
        print!("{}", template.render(&records)?);
        return Ok(());
    }
    if let Some(header) = output::tsv_header(no_pretty, &DF_HEADERS).filter(|_| !json) {
        println!("{header}");
    }
    let cells: Vec<Vec<Value>> = rows.iter().map(DfRow::cells).collect();
    output::emit_rows(json, no_pretty, &DF_HEADERS, &cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> ward_core::pb::DaemonInfo {
        ward_core::pb::DaemonInfo {
            version: "1.2.3".into(),
            platform: "linux".into(),
            backend: "krunvm".into(),
            arch: "aarch64".into(),
            hostname: "box".into(),
            kernel_version: "6.1.0".into(),
            operating_system: "Debian".into(),
            ncpu: 4,
            mem_total_bytes: 8_000_000_000,
            root_dir: "/var/lib/ward".into(),
            id: "abc".into(),
        }
    }

    fn sandbox(status: SandboxStatus, paused: bool, image: &str) -> ward_core::pb::SandboxInfo {
        ward_core::pb::SandboxInfo {
            status: status as i32,
            paused,
            image: image.into(),
            ..Default::default()
        }
    }

    #[test]
    fn given_daemon_when_version_text_built_then_both_halves_print() {
        let text = version_text(&version_record(Some(&daemon())));

        assert!(text.starts_with(&format!("Client:\n Version:  {CLIENT_VERSION}\n")));
        assert!(text.contains("\nServer:\n Version:  1.2.3\n OS/Arch:  linux/aarch64\n"));
    }

    #[test]
    fn given_no_daemon_when_version_text_built_then_only_the_client_prints() {
        let text = version_text(&version_record(None));

        assert!(!text.contains("Server"));
    }

    #[test]
    fn given_format_when_rendered_then_nested_fields_resolve() {
        let Value::Object(map) = version_record(Some(&daemon())) else {
            unreachable!()
        };
        let template = Template::parse(
            "{{.Server.Version}}/{{.Client.Version}}",
            &[(ANY_FIELD, "")],
        )
        .expect("parse");

        let rendered = template.render(&[map]).unwrap();

        assert_eq!(rendered, format!("1.2.3/{CLIENT_VERSION}\n"));
    }

    #[test]
    fn given_paused_and_stopped_sandboxes_when_info_built_then_states_are_counted_apart() {
        let sandboxes = [
            sandbox(SandboxStatus::Running, false, "a"),
            sandbox(SandboxStatus::Running, true, "a"),
            sandbox(SandboxStatus::Stopped, false, "a"),
        ];

        let record = info_record(&daemon(), &sandboxes, 2);

        assert_eq!(record["Containers"], 3);
        assert_eq!(record["ContainersRunning"], 1);
        assert_eq!(record["ContainersPaused"], 1);
        assert_eq!(record["ContainersStopped"], 1);
        assert_eq!(record["Name"], "box");
        assert_eq!(record["NCPU"], 4);
        assert_eq!(record["DockerRootDir"], "/var/lib/ward");
    }

    #[test]
    fn given_a_record_when_info_text_built_then_sections_and_host_facts_print() {
        let text = info_text(&info_record(&daemon(), &[], 0));

        assert!(
            text.starts_with("Server:\n Containers: 0\n  Running: 0\n"),
            "{text}"
        );
        assert!(text.contains(" Kernel Version: 6.1.0\n"), "{text}");
        assert!(text.contains(" Total Memory: 8GB\n"), "{text}");
        assert!(text.contains(" Docker Root Dir: /var/lib/ward\n"), "{text}");
    }

    #[test]
    fn given_images_and_sandboxes_when_df_rows_built_then_unused_images_are_reclaimable() {
        let used = ward_core::pb::ImageInfo {
            reference: "alpine:latest".into(),
            size_bytes: 3_000_000,
            ..Default::default()
        };
        let idle = ward_core::pb::ImageInfo {
            reference: "busybox:latest".into(),
            size_bytes: 1_000_000,
            ..Default::default()
        };
        let sandboxes = [sandbox(SandboxStatus::Running, false, "alpine:latest")];

        let rows = df_rows(&sandboxes, &[used, idle], &[]);

        assert_eq!(rows[0].kind, KIND_IMAGES);
        assert_eq!((rows[0].total, rows[0].active), (2, 1));
        assert_eq!(rows[0].size, "4MB");
        assert_eq!(rows[0].reclaimable, "1MB (25%)");
        assert_eq!((rows[1].total, rows[1].active), (1, 1));
        assert_eq!(rows[3].kind, KIND_BUILD_CACHE);
    }

    #[test]
    fn given_volumes_when_df_rows_built_then_all_of_their_size_is_reclaimable() {
        let volume = ward_core::pb::VolumeInfo {
            size_mb: 10,
            ..Default::default()
        };

        let rows = df_rows(&[], &[], &[volume]);

        assert_eq!(rows[2].kind, KIND_VOLUMES);
        assert!(
            rows[2].reclaimable.ends_with("(100%)"),
            "{}",
            rows[2].reclaimable
        );
    }

    #[test]
    fn given_a_df_row_when_rendered_with_the_default_format_then_the_header_is_docker_s() {
        let template = Template::parse(DF_DEFAULT_FORMAT, &DF_FIELDS).expect("parse");
        let records: Vec<_> = df_rows(&[], &[], &[]).iter().map(DfRow::record).collect();

        let text = template.render(&records).expect("render");

        assert!(
            text.starts_with("TYPE            TOTAL   ACTIVE   SIZE   RECLAIMABLE\n"),
            "{text}"
        );
    }

    #[test]
    fn given_a_paused_sandbox_when_df_rows_built_then_it_is_not_active() {
        let sandboxes = [
            sandbox(SandboxStatus::Running, true, "a"),
            sandbox(SandboxStatus::Running, false, "a"),
        ];

        let rows = df_rows(&sandboxes, &[], &[]);

        assert_eq!((rows[1].total, rows[1].active), (2, 1));
    }

    #[test]
    fn given_no_images_when_df_rows_built_then_the_row_has_no_percentage() {
        let rows = df_rows(&[], &[], &[]);

        assert_eq!(rows[0].reclaimable, "0B");
        assert_eq!(rows[2].reclaimable, "0B");
    }

    #[test]
    fn given_a_df_row_when_cells_built_then_they_follow_the_header_order() {
        let row = DfRow {
            kind: KIND_IMAGES,
            total: 3,
            active: 1,
            size: "1MB".into(),
            reclaimable: "0B".into(),
        };

        assert_eq!(
            row.cells(),
            [
                json!("Images"),
                json!(3),
                json!(1),
                json!("1MB"),
                json!("0B")
            ]
        );
        assert_eq!(row.cells().len(), DF_HEADERS.len());
    }

    #[test]
    fn given_an_empty_record_when_info_text_built_then_it_prints_blanks_without_panicking() {
        let text = info_text(&serde_json::Map::new());

        assert!(text.contains(" Name: \n"), "{text}");
        assert!(text.contains(" Total Memory: 0B\n"), "{text}");
    }
}
