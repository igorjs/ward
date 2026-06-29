// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward – command-line interface to the Ward daemon.

mod client;
mod output;
mod socket;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Top-level CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    name = "ward",
    about = "Manage Ward sandboxes",
    version,
    propagate_version = true
)]
struct Cli {
    /// Unix socket path of the ward daemon.
    #[arg(long, env = "WARD_SOCKET", global = true)]
    socket: Option<String>,

    /// Emit machine-parseable JSON instead of the default key/value or
    /// tab-separated output. One JSON object per command; list commands
    /// emit a JSON array. Stable surface for `jq` pipelines. Also
    /// honoured via `WARD_JSON=1` (any non-empty value except `0` or
    /// `false`).
    #[arg(long, global = true)]
    json: bool,

    /// Force tab-separated output for list commands even when stdout is
    /// a TTY. Useful when the operator wants to copy the table cleanly
    /// or pipe through an external pretty-printer. Also honoured via
    /// `WARD_NO_PRETTY=1`.
    #[arg(long, global = true)]
    no_pretty: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a new sandbox.
    Create {
        /// OCI image reference.
        image: String,
        /// Environment variables in KEY=VALUE form.
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Memory limit in MiB.
        #[arg(long, default_value = "512")]
        memory: u32,
        /// CPU count.
        #[arg(long, default_value = "1")]
        cpus: u32,
        /// Timeout in seconds (0 = no timeout).
        #[arg(long, default_value = "0")]
        timeout: u64,
        /// Cross-sandbox communication mode: "deny" (default) or "group".
        /// In group mode, sandboxes with identical --comms-group strings
        /// can publish/subscribe to each other.
        #[arg(long, default_value = "deny")]
        comms_mode: String,
        /// Group name for --comms-mode=group. Ignored in deny mode.
        #[arg(long, default_value = "")]
        comms_group: String,
    },

    /// List all sandboxes.
    List,

    /// Execute a command inside a sandbox.
    Exec {
        /// Sandbox ID.
        id: String,
        /// Command and arguments.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Working directory.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
    },

    /// Run a language snippet inside a sandbox.
    Run {
        /// Sandbox ID.
        id: String,
        /// Language name (python, node, deno, ruby, go).
        #[arg(short, long)]
        language: String,
        /// Inline code string to execute.
        #[arg(short, long)]
        code: String,
    },

    /// Stream stdout/stderr from a process.
    Logs {
        /// Sandbox ID.
        id: String,
        /// Process ID returned by exec/run.
        pid: String,
    },

    /// Write bytes to a process's stdin.
    Stdin {
        /// Sandbox ID.
        id: String,
        /// Process ID returned by exec/run.
        pid: String,
        /// Inline data. Use `-` (or omit) to read from the user's stdin
        /// instead — handy for piping file contents or interactive use.
        data: Option<String>,
    },

    /// Signal a process to terminate.
    Kill {
        /// Sandbox ID.
        id: String,
        /// Process ID returned by exec/run.
        pid: String,
    },

    /// Snapshot management subcommands.
    #[command(subcommand)]
    Snapshot(SnapshotCommands),

    /// Remove a sandbox.
    Remove {
        /// Sandbox ID.
        id: String,
    },

    /// Volume management subcommands.
    #[command(subcommand)]
    Volume(VolumeCommands),

    /// Publish a message to a pub/sub topic from a sandbox.
    Publish {
        /// Sandbox ID this message is published as.
        sandbox_id: String,
        /// Topic name (e.g. "agent.results.build"). Dotted segments are
        /// allowed, no leading/trailing/repeat dots.
        topic: String,
        /// Inline payload. For binary or stdin payloads, future flags
        /// (`--file`, `-`) will route bytes here.
        #[arg(default_value = "")]
        payload: String,
    },

    /// Subscribe to a pub/sub topic. Streams messages until interrupted.
    Subscribe {
        /// Sandbox ID this subscription belongs to.
        sandbox_id: String,
        /// Topic name to subscribe to.
        topic: String,
    },

    /// Show daemon health.
    Health,

    /// Show daemon information.
    Info,
}

#[derive(Debug, Subcommand)]
enum VolumeCommands {
    /// Create a named volume.
    Create {
        /// Volume name.
        name: String,
        /// Size in MiB.
        #[arg(long, default_value = "1024")]
        size: u32,
    },
    /// List all volumes.
    List,
    /// Remove a volume.
    Remove {
        /// Volume ID.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum SnapshotCommands {
    /// Create a snapshot of a sandbox.
    Create {
        /// Sandbox ID to snapshot.
        sandbox_id: String,
        /// Human-readable label for the snapshot.
        #[arg(short, long, default_value = "")]
        label: String,
    },
    /// Restore a sandbox from a snapshot.
    Restore {
        /// Sandbox ID to restore into.
        sandbox_id: String,
        /// Snapshot ID to restore from.
        snapshot_id: String,
    },
    /// List snapshots for a sandbox.
    List {
        /// Sandbox ID whose snapshots to list.
        sandbox_id: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `--json` / `--no-pretty` accept either the flag or a truthy env
/// var. clap's built-in `env` for bool fields only accepts the literal
/// strings `"true"` / `"false"`, which collides with the Unix idiom of
/// `WARD_JSON=1`. We resolve both forms here so users can either pass
/// the flag or set the env var to any non-empty value that is not
/// `"0"` or `"false"`.
fn flag_or_env(flag: bool, env_var: &str) -> bool {
    if flag {
        return true;
    }
    matches!(
        std::env::var(env_var).as_deref().map(str::trim),
        Ok(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let json = flag_or_env(cli.json, "WARD_JSON");
    let no_pretty = flag_or_env(cli.no_pretty, "WARD_NO_PRETTY");

    let socket_path = cli.socket.unwrap_or_else(socket::default_socket);

    match cli.command {
        Commands::Create {
            image,
            env,
            memory,
            cpus,
            timeout,
            comms_mode,
            comms_group,
        } => {
            // Parse KEY=VALUE env strings into a HashMap. Splitting on the
            // first '=' lets values themselves contain '=', which matters
            // for tokens, URLs, and serialised JSON.
            let env_map: std::collections::HashMap<String, String> = env
                .iter()
                .filter_map(|s| {
                    s.split_once('=')
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect();

            // Map the user-facing string to the pb enum. Unknown values
            // bail out before the wire so the user gets a clear local
            // error instead of an opaque InvalidArgument from the daemon.
            let comms_mode_pb = match comms_mode.as_str() {
                "deny" => ward_core::pb::CommunicationMode::Deny,
                "group" => ward_core::pb::CommunicationMode::Group,
                other => anyhow::bail!("--comms-mode must be 'deny' or 'group', got '{other}'"),
            };

            let mut c = client::connect(&socket_path).await?;
            let resp = c
                .create_sandbox(ward_core::pb::CreateSandboxRequest {
                    image,
                    resources: Some(ward_core::pb::ResourceLimits {
                        cpus,
                        memory_mb: memory,
                        pids_max: 0, // 0 == "use daemon default"
                        timeout_seconds: timeout,
                    }),
                    env: env_map,
                    comms: Some(ward_core::pb::CommunicationPolicy {
                        mode: comms_mode_pb as i32,
                        group: comms_group,
                    }),
                    ..Default::default()
                })
                .await?
                .into_inner();

            if json {
                println!(
                    "{}",
                    json!({
                        "id": resp.id,
                        "status": status_name(resp.status),
                        "image": resp.image,
                        "ip_address": if resp.ip_address.is_empty() { Value::Null } else { Value::String(resp.ip_address) },
                    })
                );
            } else {
                // One field per line for grep-friendly E2E assertions.
                println!("id: {}", resp.id);
                println!("status: {}", status_name(resp.status));
                println!("image: {}", resp.image);
                if !resp.ip_address.is_empty() {
                    println!("ip_address: {}", resp.ip_address);
                }
            }
        }

        Commands::List => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c.list_sandboxes(()).await?.into_inner();
            let rows: Vec<Vec<Value>> = resp
                .sandboxes
                .into_iter()
                .map(|s| {
                    vec![
                        Value::String(s.id),
                        Value::String(status_name(s.status).to_string()),
                        Value::String(s.image),
                    ]
                })
                .collect();
            output::emit_rows(json, no_pretty, &["id", "status", "image"], &rows)?;
        }

        Commands::Exec {
            id,
            command,
            workdir,
        } => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c
                .exec(ward_core::pb::ExecRequest {
                    sandbox_id: id,
                    command,
                    working_dir: workdir.unwrap_or_default(),
                    env: Default::default(),
                })
                .await?
                .into_inner();
            // pid is the handle the user passes back to `ward logs <id> <pid>`
            // to retrieve streamed output once StreamOutput is implemented.
            if json {
                println!("{}", json!({"pid": resp.pid, "status": resp.status}));
            } else {
                println!("pid: {}", resp.pid);
                println!("status: {}", resp.status);
            }
        }

        Commands::Run { id, language, code } => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c
                .run(ward_core::pb::RunRequest {
                    sandbox_id: id,
                    language,
                    code,
                })
                .await?
                .into_inner();
            if json {
                println!("{}", json!({"pid": resp.pid, "status": resp.status}));
            } else {
                println!("pid: {}", resp.pid);
                println!("status: {}", resp.status);
            }
        }

        Commands::Logs { id, pid } => {
            // StreamOutput is a server-streaming RPC. The CLI drains the
            // stream and prints each event with a stable prefix so scripts
            // can grep "stdout:" / "stderr:" / "exit:" without parsing
            // structured output.
            let mut c = client::connect(&socket_path).await?;
            let mut stream = c
                .stream_output(ward_core::pb::StreamOutputRequest {
                    sandbox_id: id,
                    pid,
                })
                .await?
                .into_inner();

            while let Some(evt) = stream.message().await? {
                let kind = match ward_core::pb::StreamEventType::try_from(evt.r#type)
                    .unwrap_or(ward_core::pb::StreamEventType::Unspecified)
                {
                    ward_core::pb::StreamEventType::Stdout => "stdout",
                    ward_core::pb::StreamEventType::Stderr => "stderr",
                    ward_core::pb::StreamEventType::Exit => "exit",
                    ward_core::pb::StreamEventType::Unspecified => "unspecified",
                };
                if json {
                    // One JSON object per event (JSON Lines / NDJSON);
                    // `jq -c .` is the natural consumer for streaming.
                    if kind == "exit" {
                        println!("{}", json!({"type": "exit", "exit_code": evt.exit_code}));
                    } else {
                        println!("{}", json!({"type": kind, "line": evt.line}));
                    }
                } else if kind == "exit" {
                    println!("exit: {}", evt.exit_code);
                } else {
                    println!("{kind}: {}", evt.line);
                }
            }
        }

        Commands::Kill { id, pid } => {
            let mut c = client::connect(&socket_path).await?;
            c.kill_process(ward_core::pb::KillProcessRequest {
                sandbox_id: id,
                pid: pid.clone(),
            })
            .await?;
            if json {
                println!("{}", json!({"killed": pid}));
            } else {
                println!("killed: {pid}");
            }
        }

        Commands::Stdin { id, pid, data } => {
            // Three input modes:
            //   ward stdin <id> <pid> "literal"     -> send the literal bytes
            //   ward stdin <id> <pid> -             -> read from CLI's stdin
            //   ward stdin <id> <pid>               -> same as -
            // Reading our own stdin via read_to_end is the conventional Unix
            // shape: lets users pipe files or other commands without juggling
            // shell quoting.
            let bytes = match data.as_deref() {
                Some(literal) if literal != "-" => literal.as_bytes().to_vec(),
                _ => {
                    use std::io::Read;
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf)?;
                    buf
                }
            };
            let mut c = client::connect(&socket_path).await?;
            let bytes_written = bytes.len();
            c.write_stdin(ward_core::pb::WriteStdinRequest {
                sandbox_id: id,
                pid,
                data: bytes,
            })
            .await?;
            if json {
                println!("{}", json!({"wrote": bytes_written}));
            } else {
                println!("wrote");
            }
        }

        Commands::Snapshot(snap_cmd) => match snap_cmd {
            SnapshotCommands::Create { sandbox_id, label } => {
                let mut c = client::connect(&socket_path).await?;
                let resp = c
                    .create_snapshot(ward_core::pb::CreateSnapshotRequest { sandbox_id, label })
                    .await?
                    .into_inner();
                if json {
                    println!(
                        "{}",
                        json!({
                            "snapshot_id": resp.snapshot_id,
                            "sandbox_id": resp.sandbox_id,
                            "label": resp.label,
                            "size_bytes": resp.size_bytes,
                        })
                    );
                } else {
                    println!("snapshot_id: {}", resp.snapshot_id);
                    println!("sandbox_id: {}", resp.sandbox_id);
                    println!("label: {}", resp.label);
                    println!("size_bytes: {}", resp.size_bytes);
                }
            }
            SnapshotCommands::Restore {
                sandbox_id,
                snapshot_id,
            } => {
                let mut c = client::connect(&socket_path).await?;
                c.restore_snapshot(ward_core::pb::RestoreSnapshotRequest {
                    sandbox_id: sandbox_id.clone(),
                    snapshot_id: snapshot_id.clone(),
                })
                .await?;
                if json {
                    println!(
                        "{}",
                        json!({"restored": sandbox_id, "from_snapshot": snapshot_id})
                    );
                } else {
                    println!("restored: {sandbox_id} from {snapshot_id}");
                }
            }
            SnapshotCommands::List { sandbox_id } => {
                let mut c = client::connect(&socket_path).await?;
                let resp = c
                    .list_snapshots(ward_core::pb::ListSnapshotsRequest { sandbox_id })
                    .await?
                    .into_inner();
                let rows: Vec<Vec<Value>> = resp
                    .snapshots
                    .into_iter()
                    .map(|s| {
                        vec![
                            Value::String(s.snapshot_id),
                            Value::String(s.sandbox_id),
                            Value::String(s.label),
                            Value::from(s.size_bytes),
                        ]
                    })
                    .collect();
                output::emit_rows(
                    json,
                    no_pretty,
                    &["snapshot_id", "sandbox_id", "label", "size_bytes"],
                    &rows,
                )?;
            }
        },

        Commands::Remove { id } => {
            let mut c = client::connect(&socket_path).await?;
            c.remove_sandbox(ward_core::pb::RemoveSandboxRequest { id: id.clone() })
                .await?;
            if json {
                println!("{}", json!({"removed": id}));
            } else {
                println!("removed: {id}");
            }
        }

        Commands::Volume(vol_cmd) => match vol_cmd {
            VolumeCommands::Create { name, size } => {
                let mut c = client::connect(&socket_path).await?;
                let resp = c
                    .create_volume(ward_core::pb::CreateVolumeRequest {
                        name,
                        size_mb: size,
                    })
                    .await?
                    .into_inner();
                if json {
                    println!(
                        "{}",
                        json!({
                            "id": resp.id,
                            "name": resp.name,
                            "size_mb": resp.size_mb,
                            "mount_path": resp.mount_path,
                        })
                    );
                } else {
                    println!("id: {}", resp.id);
                    println!("name: {}", resp.name);
                    println!("size_mb: {}", resp.size_mb);
                    println!("mount_path: {}", resp.mount_path);
                }
            }
            VolumeCommands::List => {
                let mut c = client::connect(&socket_path).await?;
                let resp = c.list_volumes(()).await?.into_inner();
                let rows: Vec<Vec<Value>> = resp
                    .volumes
                    .into_iter()
                    .map(|v| {
                        vec![
                            Value::String(v.id),
                            Value::String(v.name),
                            Value::from(v.size_mb),
                        ]
                    })
                    .collect();
                output::emit_rows(json, no_pretty, &["id", "name", "size_mb"], &rows)?;
            }
            VolumeCommands::Remove { id } => {
                let mut c = client::connect(&socket_path).await?;
                c.remove_volume(ward_core::pb::RemoveVolumeRequest { id: id.clone() })
                    .await?;
                if json {
                    println!("{}", json!({"removed": id}));
                } else {
                    println!("removed: {id}");
                }
            }
        },

        Commands::Publish {
            sandbox_id,
            topic,
            payload,
        } => {
            let mut c = client::connect(&socket_path).await?;
            c.publish(ward_core::pb::PublishRequest {
                sandbox_id,
                topic,
                payload: payload.into_bytes(),
            })
            .await?;
            if json {
                println!("{}", json!({"published": true}));
            } else {
                println!("published");
            }
        }

        Commands::Subscribe { sandbox_id, topic } => {
            // Subscribe is server-streaming. We drain the stream and print
            // each message until the user hits Ctrl-C or the daemon ends
            // the stream. Until the broker is implemented, the daemon
            // returns Unimplemented before any messages flow.
            let mut c = client::connect(&socket_path).await?;
            let mut stream = c
                .subscribe(ward_core::pb::SubscribeRequest { sandbox_id, topic })
                .await?
                .into_inner();

            while let Some(msg) = stream.message().await? {
                if json {
                    // One JSON object per message (NDJSON). Payload is
                    // base64-encoded so binary payloads survive the
                    // round-trip; consumers `jq -r .payload | base64 -d`
                    // to recover bytes.
                    use base64::Engine;
                    let payload_b64 =
                        base64::engine::general_purpose::STANDARD.encode(&msg.payload);
                    println!(
                        "{}",
                        json!({
                            "topic": msg.topic,
                            "from_sandbox": msg.from_sandbox,
                            "payload_b64": payload_b64,
                        })
                    );
                } else {
                    println!("topic: {}", msg.topic);
                    println!("from_sandbox: {}", msg.from_sandbox);
                    println!("payload: {}", String::from_utf8_lossy(&msg.payload));
                    println!("---");
                }
            }
        }

        Commands::Health => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c.get_health(()).await?.into_inner();
            if json {
                println!(
                    "{}",
                    json!({
                        "status": resp.status,
                        "uptime_seconds": resp.uptime_seconds,
                        "sandbox_count": resp.sandbox_count,
                    })
                );
            } else {
                println!("status: {}", resp.status);
                println!("uptime_seconds: {}", resp.uptime_seconds);
                println!("sandbox_count: {}", resp.sandbox_count);
            }
        }

        Commands::Info => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c.get_info(()).await?.into_inner();
            if json {
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
        }
    }

    Ok(())
}

/// Convert a `SandboxStatus` enum value (passed across the wire as i32)
/// into the lowercase status name the CLI prints. Unknown values map to
/// `"unspecified"` rather than panicking, so a newer daemon with an
/// added status variant still produces readable (if unfamiliar) output.
fn status_name(status: i32) -> &'static str {
    use ward_core::pb::SandboxStatus;
    match SandboxStatus::try_from(status).unwrap_or(SandboxStatus::Unspecified) {
        SandboxStatus::Creating => "creating",
        SandboxStatus::Running => "running",
        SandboxStatus::Stopped => "stopped",
        SandboxStatus::Failed => "failed",
        SandboxStatus::Unspecified => "unspecified",
    }
}
