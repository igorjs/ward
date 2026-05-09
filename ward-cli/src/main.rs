// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward – command-line interface to the Ward daemon.

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

    // TODO: establish gRPC channel to the daemon socket.
    let _socket = cli.socket.unwrap_or_else(default_socket);

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
                println!("TODO: create volume name={name} size={size}MiB");
            }
            VolumeCommands::List => {
                println!("TODO: list volumes");
            }
            VolumeCommands::Remove { id } => {
                println!("TODO: remove volume {id}");
            }
        },

        Commands::Health => {
            println!("TODO: get health");
        }

        Commands::Info => {
            println!("TODO: get info");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_socket() -> String {
    if let Ok(v) = std::env::var("WARD_SOCKET") {
        return v;
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{home}/.ward/ward.sock")
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{xdg}/ward/ward.sock")
        } else {
            let user = std::env::var("USER").unwrap_or_else(|_| "ward".to_string());
            format!("/tmp/ward-{user}/ward.sock")
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "/tmp/ward.sock".to_string()
    }
}
