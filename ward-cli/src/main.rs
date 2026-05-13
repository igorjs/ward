// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward – command-line interface to the Ward daemon.

mod client;
mod socket;

use clap::{Parser, Subcommand};

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

    /// Snapshot a sandbox.
    Snapshot {
        /// Sandbox ID.
        id: String,
        /// Human-readable label for the snapshot.
        #[arg(short, long, default_value = "")]
        label: String,
    },

    /// Restore a sandbox from a snapshot.
    Restore {
        /// Sandbox ID.
        id: String,
        /// Snapshot ID to restore from.
        snapshot_id: String,
    },

    /// Remove a sandbox.
    Remove {
        /// Sandbox ID.
        id: String,
    },

    /// Volume management subcommands.
    #[command(subcommand)]
    Volume(VolumeCommands),

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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let socket_path = cli.socket.unwrap_or_else(socket::default_socket);

    match cli.command {
        Commands::Create {
            image,
            env,
            memory,
            cpus,
            timeout,
        } => {
            println!(
                "TODO: create sandbox image={image} memory={memory}MiB cpus={cpus} timeout={timeout}s env={env:?}"
            );
        }

        Commands::List => {
            println!("TODO: list sandboxes");
        }

        Commands::Exec {
            id,
            command,
            workdir,
        } => {
            println!("TODO: exec in sandbox {id} command={command:?} workdir={workdir:?}");
        }

        Commands::Run { id, language, code } => {
            println!("TODO: run {language} in sandbox {id} code={code:?}");
        }

        Commands::Logs { id, pid } => {
            println!("TODO: stream logs for sandbox={id} pid={pid}");
        }

        Commands::Snapshot { id, label } => {
            println!("TODO: snapshot sandbox {id} label={label}");
        }

        Commands::Restore { id, snapshot_id } => {
            println!("TODO: restore sandbox {id} from snapshot {snapshot_id}");
        }

        Commands::Remove { id } => {
            println!("TODO: remove sandbox {id}");
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
                // One field per line so E2E tests can grep without parsing
                // structured output. Same convention as `ward info`.
                println!("id: {}", resp.id);
                println!("name: {}", resp.name);
                println!("size_mb: {}", resp.size_mb);
                println!("mount_path: {}", resp.mount_path);
            }
            VolumeCommands::List => {
                let mut c = client::connect(&socket_path).await?;
                let resp = c.list_volumes(()).await?.into_inner();
                // Tab-separated columns: stable for `awk` / `cut` in scripts.
                // Empty output (no volumes) is the convention for "list found
                // nothing"; users distinguish "no volumes" from "command failed"
                // via the exit code, not by parsing stdout text.
                for v in resp.volumes {
                    println!("{}\t{}\t{}MiB", v.id, v.name, v.size_mb);
                }
            }
            VolumeCommands::Remove { id } => {
                let mut c = client::connect(&socket_path).await?;
                c.remove_volume(ward_core::pb::RemoveVolumeRequest { id: id.clone() })
                    .await?;
                println!("removed: {id}");
            }
        },

        Commands::Health => {
            // Connect, call GetHealth, render plain-text output so E2E
            // tests can grep simple fields without parsing structured output.
            let mut c = client::connect(&socket_path).await?;
            let resp = c.get_health(()).await?.into_inner();
            println!("status: {}", resp.status);
            println!("uptime_seconds: {}", resp.uptime_seconds);
            println!("sandbox_count: {}", resp.sandbox_count);
        }

        Commands::Info => {
            let mut c = client::connect(&socket_path).await?;
            let resp = c.get_info(()).await?.into_inner();
            println!("version: {}", resp.version);
            println!("platform: {}", resp.platform);
            println!("arch: {}", resp.arch);
            println!("backend: {}", resp.backend);
        }
    }

    Ok(())
}
