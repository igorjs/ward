// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward version`, `ward system info` and `ward system df`.

use anyhow::Result;
use serde_json::{Value, json};
use ward_core::pb::SandboxStatus;

use crate::images::size_text;
use crate::template::{ANY_FIELD, Template};
use crate::{client, output};

const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const INDENT: &str = " ";
/// Shown for a figure Ward does not measure, as `ward stats` does.
const UNMEASURED: &str = "--";
const DF_HEADERS: [&str; 4] = ["TYPE", "TOTAL", "ACTIVE", "SIZE"];

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
    if let Some(template) = template {
        let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
        let images = c.list_images(()).await?.into_inner().images;
        let record = info_record(&resp, &sandboxes, images.len());
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
        println!("version: {}", resp.version);
        println!("platform: {}", resp.platform);
        println!("arch: {}", resp.arch);
        println!("backend: {}", resp.backend);
    }
    Ok(())
}

/// The `docker info` record with the fields Ward has, under Docker's names.
fn info_record(
    info: &ward_core::pb::DaemonInfo,
    sandboxes: &[ward_core::pb::SandboxInfo],
    images: usize,
) -> serde_json::Map<String, Value> {
    let running = sandboxes
        .iter()
        .filter(|s| s.status == SandboxStatus::Running as i32)
        .count();
    let Value::Object(map) = json!({
        "ServerVersion": info.version,
        "OSType": info.platform,
        "Architecture": info.arch,
        "Driver": info.backend,
        "Containers": sandboxes.len(),
        "ContainersRunning": running,
        "ContainersStopped": sandboxes.len() - running,
        "Images": images,
    }) else {
        unreachable!("json! of an object")
    };
    map
}

/// Counts sandboxes and cached images, like `docker system df`.
pub async fn df(socket: &str, json: bool, no_pretty: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let sandboxes = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let images = c.list_images(()).await?.into_inner().images;
    let running = sandboxes
        .iter()
        .filter(|s| s.status == SandboxStatus::Running as i32)
        .count();
    let cache_bytes: u64 = images.iter().map(|image| image.size_bytes).sum();
    let row = |kind: &str, total: usize, active: Value, size: String| {
        vec![kind.into(), total.into(), active, size.into()]
    };
    let rows = vec![
        row(
            "Images",
            images.len(),
            UNMEASURED.into(),
            size_text(cache_bytes),
        ),
        row(
            "Containers",
            sandboxes.len(),
            running.into(),
            UNMEASURED.into(),
        ),
    ];
    if let Some(header) = output::tsv_header(no_pretty, &DF_HEADERS).filter(|_| !json) {
        println!("{header}");
    }
    output::emit_rows(json, no_pretty, &DF_HEADERS, &rows)
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
}
