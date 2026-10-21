// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward – command-line interface to the Ward daemon.

mod attach;
mod client;
mod docker;
mod inspect;
mod lifecycle;
mod output;
mod resources;
mod socket;
mod terminal;
mod tokens;

use clap::{Args, Parser, Subcommand};
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

const DEFAULT_MEMORY: &str = "512m";
const DEFAULT_CPUS: &str = "1";

/// Sandbox settings shared by `create` and `run`.
#[derive(Debug, Args)]
struct SandboxOpts {
    /// Name to refer to the sandbox by instead of its ID.
    #[arg(long)]
    name: Option<String>,
    /// Environment variables in KEY=VALUE form.
    #[arg(short, long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Share a host directory into the sandbox: SOURCE:TARGET[:ro].
    #[arg(short = 'v', long = "volume", value_name = "SOURCE:TARGET[:ro]")]
    volume: Vec<String>,
    /// Outbound network policy: "deny" (default), "allowlist" or "open".
    #[arg(long, default_value = "deny")]
    egress: String,
    /// Host name the sandbox may reach with --egress=allowlist. Repeatable.
    #[arg(long = "allow-domain", value_name = "DOMAIN")]
    allow_domain: Vec<String>,
    /// Memory limit: bytes, or a number with a k, m, g or t suffix (512m,
    /// 1.5g). 0 uses the daemon default.
    #[arg(short = 'm', long, default_value = DEFAULT_MEMORY, value_name = "SIZE")]
    memory: String,
    /// CPU limit. Fractions round up to a whole CPU. 0 uses the daemon default.
    #[arg(long, default_value = DEFAULT_CPUS, value_name = "NUMBER")]
    cpus: String,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a new sandbox without starting a command, and print its ID.
    Create {
        /// OCI image reference.
        image: String,
        /// Command and arguments the sandbox runs.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
        #[command(flatten)]
        opts: SandboxOpts,
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
        /// Snapshot ID to restore into the new sandbox. Requires
        /// --source-sandbox to authorize the restore.
        #[arg(long, requires = "source_sandbox")]
        from_snapshot: Option<String>,
        /// Sandbox whose stored token authorizes --from-snapshot. Ignored
        /// unless --from-snapshot is also set.
        #[arg(long)]
        source_sandbox: Option<String>,
    },

    /// List all sandboxes.
    List,

    /// List running sandboxes, or every sandbox with -a.
    Ps {
        /// Show sandboxes in every state.
        #[arg(short, long)]
        all: bool,
        /// Print only the sandbox IDs.
        #[arg(short, long)]
        quiet: bool,
        /// Output format. Only `json` is supported.
        #[arg(long, value_parser = ["json"])]
        format: Option<String>,
    },

    /// Print low level details of one or more sandboxes as a JSON array.
    Inspect {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        references: Vec<String>,
    },

    /// Execute a command inside a sandbox.
    ///
    /// The command's output is printed and the CLI exits with its exit
    /// code. With -d it starts in the background and prints its PID.
    Exec {
        /// Sandbox name, ID or ID prefix.
        id: String,
        /// Command and arguments.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Working directory.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// Environment variables in KEY=VALUE form.
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Keep stdin open and send it to the command.
        #[arg(short, long)]
        interactive: bool,
        /// Run the command on a pseudo-terminal.
        #[arg(short, long)]
        tty: bool,
        /// Start the command in the background and print its PID. Ignores
        /// -i and -t, since nothing attaches to the command.
        #[arg(short, long)]
        detach: bool,
    },

    /// Create a sandbox, run a command in it and exit with its exit code.
    Run {
        /// OCI image reference.
        image: String,
        /// Command and arguments.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
        #[command(flatten)]
        opts: SandboxOpts,
        /// Working directory.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// Keep stdin open and send it to the command.
        #[arg(short, long)]
        interactive: bool,
        /// Run the command on a pseudo-terminal.
        #[arg(short, long)]
        tty: bool,
        /// Remove the sandbox when the command ends.
        #[arg(long, conflicts_with = "detach")]
        rm: bool,
        /// Print the sandbox ID and leave the command running.
        #[arg(short, long)]
        detach: bool,
    },

    /// Run a language snippet inside a sandbox.
    RunCode {
        /// Sandbox ID.
        id: String,
        /// Language name (python, node, deno, ruby, go).
        #[arg(short, long)]
        language: String,
        /// Inline code string to execute.
        #[arg(short, long)]
        code: String,
    },

    /// Print a sandbox's output, or stream a process by its PID.
    Logs {
        /// Sandbox name, ID or ID prefix.
        id: String,
        /// Process ID returned by exec. Without it the main process output
        /// is printed.
        pid: Option<String>,
        /// Keep streaming until the main process ends.
        #[arg(short, long, conflicts_with = "pid")]
        follow: bool,
        /// Number of lines to show from the end of the output, or "all".
        #[arg(short = 'n', long, default_value = docker::TAIL_ALL, conflicts_with = "pid")]
        tail: String,
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

    /// Signal a process, by default the sandbox's main process.
    Kill {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        refs: Vec<String>,
        /// Process ID returned by exec/run. 0 means the main process.
        #[arg(long, default_value = ward_core::sandbox::MAIN_PROCESS_PID)]
        pid: String,
        /// Signal for the main process, by name or number.
        #[arg(short, long, default_value = "KILL")]
        signal: String,
    },

    /// Snapshot management subcommands.
    #[command(subcommand)]
    Snapshot(SnapshotCommands),

    /// Stop one or more sandboxes.
    Stop {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        refs: Vec<String>,
    },

    /// Start one or more stopped sandboxes.
    Start {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        refs: Vec<String>,
    },

    /// Restart one or more sandboxes.
    Restart {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        refs: Vec<String>,
    },

    /// Remove one or more sandboxes.
    #[command(alias = "rm")]
    Remove {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        refs: Vec<String>,
        /// Remove a running sandbox too.
        #[arg(short, long)]
        force: bool,
    },

    /// Remove every stopped and exited sandbox.
    Prune,

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

/// Parse `SOURCE:TARGET[:ro]` from `--volume`.
fn parse_mount_spec(spec: &str) -> anyhow::Result<ward_core::pb::Mount> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, readonly) = match parts.as_slice() {
        [source, target] => (*source, *target, false),
        [source, target, "ro"] => (*source, *target, true),
        [source, target, "rw"] => (*source, *target, false),
        _ => anyhow::bail!("--volume expects SOURCE:TARGET[:ro|rw], got '{spec}'"),
    };
    Ok(ward_core::pb::Mount {
        source: source.to_string(),
        target: target.to_string(),
        readonly,
    })
}

/// Parse repeated `KEY=VALUE` flags. Splitting on the first '=' lets values
/// themselves contain '=', which matters for tokens, URLs and serialised JSON.
/// A bare `KEY` takes its value from the host and is dropped when unset,
/// like Docker's `-e KEY`.
fn parse_env(pairs: &[String]) -> std::collections::HashMap<String, String> {
    parse_env_with(pairs, |key| std::env::var(key).ok())
}

fn parse_env_with(
    pairs: &[String],
    host: impl Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, String> {
    pairs
        .iter()
        .filter_map(|s| match s.split_once('=') {
            Some((k, v)) => Some((k.to_string(), v.to_string())),
            None => host(s).map(|v| (s.clone(), v)),
        })
        .collect()
}

/// `ward run` has no timeout flag; zero lets the daemon apply its default.
const DAEMON_DEFAULT_TIMEOUT_SECONDS: u64 = 0;

/// Build the create request for the settings `create` and `run` share.
fn create_request(
    image: String,
    opts: SandboxOpts,
    timeout_seconds: u64,
) -> anyhow::Result<ward_core::pb::CreateSandboxRequest> {
    // Unknown values bail out before the wire so the user gets a clear
    // local error instead of an opaque InvalidArgument from the daemon.
    let egress_mode = match opts.egress.as_str() {
        "deny" => ward_core::pb::EgressMode::Deny,
        "allowlist" => ward_core::pb::EgressMode::Allowlist,
        "open" => ward_core::pb::EgressMode::Open,
        other => anyhow::bail!("--egress must be 'deny', 'allowlist' or 'open', got '{other}'"),
    };
    let resources = ward_core::pb::ResourceLimits {
        cpus: resources::parse_cpus(&opts.cpus)?,
        memory_mb: resources::parse_memory_mib(&opts.memory)?,
        pids_max: 0, // 0 == "use daemon default"
        timeout_seconds,
    };
    let mounts = opts
        .volume
        .iter()
        .map(|spec| parse_mount_spec(spec))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(ward_core::pb::CreateSandboxRequest {
        image,
        mounts,
        egress: Some(ward_core::pb::EgressPolicy {
            mode: egress_mode as i32,
            domains: opts.allow_domain,
        }),
        resources: Some(resources),
        env: parse_env(&opts.env),
        name: opts.name.unwrap_or_default(),
        ..Default::default()
    })
}

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

/// Docker's exit code for a failure of the CLI or daemon itself, as opposed
/// to the exit code of the command that ran.
const CLI_FAILURE_EXIT_CODE: i32 = 125;
const GENERIC_FAILURE_EXIT_CODE: i32 = 1;

/// The first positional argument, which names the subcommand. Skips flags and
/// the value of `--socket`, the one global flag that takes a separate value.
fn subcommand_name(args: impl IntoIterator<Item = String>) -> Option<String> {
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            args.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

/// `run` and `exec` report their own failures as 125, so that scripts can tell
/// them apart from the exit code of the command they ran.
fn reports_cli_failure_as_125(subcommand: Option<&str>) -> bool {
    matches!(subcommand, Some("run" | "exec"))
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr()
                && reports_cli_failure_as_125(subcommand_name(std::env::args()).as_deref())
            {
                CLI_FAILURE_EXIT_CODE
            } else {
                e.exit_code()
            };
            let _ = e.print();
            std::process::exit(code);
        }
    };
    let failure_code = if matches!(cli.command, Commands::Run { .. } | Commands::Exec { .. }) {
        CLI_FAILURE_EXIT_CODE
    } else {
        GENERIC_FAILURE_EXIT_CODE
    };
    if let Err(e) = run_cli(cli).await {
        eprintln!("Error: {e:?}");
        std::process::exit(failure_code);
    }
}

async fn run_cli(cli: Cli) -> anyhow::Result<()> {
    let json = flag_or_env(cli.json, "WARD_JSON");
    let no_pretty = flag_or_env(cli.no_pretty, "WARD_NO_PRETTY");

    let socket_path = cli.socket.unwrap_or_else(socket::default_socket);
    // Computed once: every command below that touches a token reads or
    // writes the same per-daemon store file.
    let token_store = tokens::store_path(&socket_path);

    match cli.command {
        Commands::Create {
            image,
            command,
            opts,
            timeout,
            comms_mode,
            comms_group,
            from_snapshot,
            source_sandbox,
        } => {
            let comms_mode_pb = match comms_mode.as_str() {
                "deny" => ward_core::pb::CommunicationMode::Deny,
                "group" => ward_core::pb::CommunicationMode::Group,
                other => anyhow::bail!("--comms-mode must be 'deny' or 'group', got '{other}'"),
            };
            let mut req = create_request(image, opts, timeout)?;
            req.command = command;
            req.comms = Some(ward_core::pb::CommunicationPolicy {
                mode: comms_mode_pb as i32,
                group: comms_group,
            });
            req.from_snapshot = from_snapshot.clone().unwrap_or_default();
            let mut c = client::connect(&socket_path).await?;

            let resp = if let (Some(_), Some(source_sandbox)) = (&from_snapshot, &source_sandbox) {
                // Restoring from a snapshot requires the source sandbox's
                // token to authorize the daemon's from_snapshot gate.
                let source_token = tokens::load(&token_store, source_sandbox)?;
                let mut request = tonic::Request::new(req);
                tokens::attach_token(&mut request, source_token)?;
                c.create_sandbox(request).await?.into_inner()
            } else {
                c.create_sandbox(req).await?.into_inner()
            };

            // The sandbox already exists on the daemon, and without its
            // token no later command can manage or remove it, so the token
            // is shown whenever the store cannot keep it.
            let saved = tokens::save(&token_store, &resp.id, &resp.token);
            if json {
                println!(
                    "{}",
                    json!({
                        "id": resp.id,
                        "status": status_name(resp.status),
                        "image": resp.image,
                        "ip_address": if resp.ip_address.is_empty() { Value::Null } else { Value::String(resp.ip_address) },
                        "token": resp.token,
                    })
                );
            } else {
                println!("{}", resp.id);
            }
            if let Err(e) = saved {
                eprintln!("warning: could not persist token locally: {e}");
                if !json {
                    eprintln!("token: {}", resp.token);
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

        Commands::Ps { all, quiet, format } => {
            let mut c = client::connect(&socket_path).await?;
            let options = inspect::PsOptions {
                all,
                quiet,
                json,
                format_json: format.is_some(),
                no_pretty,
            };
            inspect::ps(&mut c, options).await?;
        }

        Commands::Inspect { references } => {
            let mut c = client::connect(&socket_path).await?;
            inspect::inspect(&mut c, &token_store, &references).await?;
        }

        Commands::Exec {
            id,
            command,
            workdir,
            env,
            interactive,
            tty,
            detach,
        } => {
            if !detach {
                let spec = docker::ExecSpec {
                    command,
                    env: parse_env(&env),
                    workdir: workdir.unwrap_or_default(),
                    interactive,
                    tty,
                };
                let code = docker::exec(&socket_path, &token_store, &id, spec).await?;
                std::process::exit(code);
            }
            let mut c = client::connect(&socket_path).await?;
            let target = client::resolve(&mut c, &token_store, &id).await?;
            let request = target.request(ward_core::pb::ExecRequest {
                sandbox_id: target.id.clone(),
                command,
                working_dir: workdir.unwrap_or_default(),
                env: parse_env(&env),
            })?;
            let resp = c.exec(request).await?.into_inner();
            // pid is the handle the user passes back to `ward logs <id> <pid>`.
            if json {
                println!("{}", json!({"pid": resp.pid, "status": resp.status}));
            } else {
                println!("pid: {}", resp.pid);
                println!("status: {}", resp.status);
            }
        }

        Commands::Run {
            image,
            command,
            opts,
            workdir,
            interactive,
            tty,
            rm,
            detach,
        } => {
            let mut request = create_request(image, opts, DAEMON_DEFAULT_TIMEOUT_SECONDS)?;
            request.command = command;
            request.tty = tty;
            request.workdir = workdir.unwrap_or_default();
            let flags = docker::RunFlags {
                interactive,
                tty,
                rm,
                detach,
            };
            let code = docker::run(&socket_path, &token_store, request, flags).await?;
            std::process::exit(code);
        }

        Commands::RunCode { id, language, code } => {
            let mut c = client::connect(&socket_path).await?;
            let token = tokens::load(&token_store, &id)?;
            let mut request = tonic::Request::new(ward_core::pb::RunRequest {
                sandbox_id: id,
                language,
                code,
            });
            tokens::attach_token(&mut request, token)?;
            let resp = c.run(request).await?.into_inner();
            if json {
                println!("{}", json!({"pid": resp.pid, "status": resp.status}));
            } else {
                println!("pid: {}", resp.pid);
                println!("status: {}", resp.status);
            }
        }

        Commands::Logs {
            id,
            pid: None,
            follow,
            tail,
        } => {
            let tail = docker::parse_tail(&tail)?;
            let code = docker::logs(&socket_path, &token_store, &id, follow, tail).await?;
            if code != 0 {
                std::process::exit(code);
            }
        }

        Commands::Logs {
            id, pid: Some(pid), ..
        } => {
            // StreamOutput is a server-streaming RPC. The CLI drains the
            // stream and prints each event with a stable prefix so scripts
            // can grep "stdout:" / "stderr:" / "exit:" without parsing
            // structured output.
            let mut c = client::connect(&socket_path).await?;
            let target = client::resolve(&mut c, &token_store, &id).await?;
            let request = target.request(ward_core::pb::StreamOutputRequest {
                sandbox_id: target.id.clone(),
                pid,
            })?;
            let mut stream = c.stream_output(request).await?.into_inner();

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

        Commands::Kill { refs, pid, signal } => {
            let signal = inspect::parse_signal(&signal)?;
            let mut c = client::connect(&socket_path).await?;
            inspect::kill(&mut c, &token_store, &refs, &pid, signal, json).await?;
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
            let token = tokens::load(&token_store, &id)?;
            let bytes_written = bytes.len();
            let mut request = tonic::Request::new(ward_core::pb::WriteStdinRequest {
                sandbox_id: id,
                pid,
                data: bytes,
            });
            tokens::attach_token(&mut request, token)?;
            c.write_stdin(request).await?;
            if json {
                println!("{}", json!({"wrote": bytes_written}));
            } else {
                println!("wrote");
            }
        }

        Commands::Snapshot(snap_cmd) => match snap_cmd {
            SnapshotCommands::Create { sandbox_id, label } => {
                let mut c = client::connect(&socket_path).await?;
                let token = tokens::load(&token_store, &sandbox_id)?;
                let mut request =
                    tonic::Request::new(ward_core::pb::CreateSnapshotRequest { sandbox_id, label });
                tokens::attach_token(&mut request, token)?;
                let resp = c.create_snapshot(request).await?.into_inner();
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
                let token = tokens::load(&token_store, &sandbox_id)?;
                let mut request = tonic::Request::new(ward_core::pb::RestoreSnapshotRequest {
                    sandbox_id: sandbox_id.clone(),
                    snapshot_id: snapshot_id.clone(),
                });
                tokens::attach_token(&mut request, token)?;
                c.restore_snapshot(request).await?;
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
                let token = tokens::load(&token_store, &sandbox_id)?;
                let mut request =
                    tonic::Request::new(ward_core::pb::ListSnapshotsRequest { sandbox_id });
                tokens::attach_token(&mut request, token)?;
                let resp = c.list_snapshots(request).await?.into_inner();
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

        Commands::Stop { refs } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::lifecycle(&mut c, &token_store, lifecycle::Action::Stop, &refs, json)
                .await?;
        }

        Commands::Start { refs } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::lifecycle(&mut c, &token_store, lifecycle::Action::Start, &refs, json)
                .await?;
        }

        Commands::Restart { refs } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::lifecycle(
                &mut c,
                &token_store,
                lifecycle::Action::Restart,
                &refs,
                json,
            )
            .await?;
        }

        Commands::Remove { refs, force } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::remove(&mut c, &token_store, &refs, force, json).await?;
        }

        Commands::Prune => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::prune(&mut c, &token_store, json).await?;
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
                    println!("{id}");
                }
            }
        },

        Commands::Publish {
            sandbox_id,
            topic,
            payload,
        } => {
            let mut c = client::connect(&socket_path).await?;
            let token = tokens::load(&token_store, &sandbox_id)?;
            let mut request = tonic::Request::new(ward_core::pb::PublishRequest {
                sandbox_id,
                topic,
                payload: payload.into_bytes(),
            });
            tokens::attach_token(&mut request, token)?;
            c.publish(request).await?;
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
            let token = tokens::load(&token_store, &sandbox_id)?;
            let mut request =
                tonic::Request::new(ward_core::pb::SubscribeRequest { sandbox_id, topic });
            tokens::attach_token(&mut request, token)?;
            let mut stream = c.subscribe(request).await?.into_inner();

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
        SandboxStatus::Exited => "exited",
        SandboxStatus::Stopping => "stopping",
        SandboxStatus::Unspecified => "unspecified",
    }
}

// ---------------------------------------------------------------------------
// Tests
//
// BDD/AAA style: function names read as `given_X_when_Y_then_Z`, bodies
// have explicit Arrange / Act / Assert markers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_source_and_target_when_parse_mount_then_read_write() {
        let m = parse_mount_spec("/tmp/a:/data").unwrap();
        assert_eq!(
            (m.source.as_str(), m.target.as_str(), m.readonly),
            ("/tmp/a", "/data", false)
        );
    }

    #[test]
    fn given_bare_key_when_parse_env_then_host_value_is_used_or_key_dropped() {
        // Arrange
        let pairs = ["SET".to_string(), "UNSET".to_string(), "A=b=c".to_string()];
        let host = |key: &str| (key == "SET").then(|| "from-host".to_string());

        // Act
        let env = parse_env_with(&pairs, host);

        // Assert
        assert_eq!(env.get("SET").map(String::as_str), Some("from-host"));
        assert!(!env.contains_key("UNSET"));
        assert_eq!(env.get("A").map(String::as_str), Some("b=c"));
    }

    #[test]
    fn given_global_flags_before_subcommand_when_named_then_subcommand_is_found() {
        // Arrange
        let args = ["ward", "--json", "--socket", "run", "exec", "x"].map(String::from);

        // Act
        let name = subcommand_name(args);

        // Assert
        assert_eq!(name.as_deref(), Some("exec"));
        assert!(reports_cli_failure_as_125(name.as_deref()));
        assert!(!reports_cli_failure_as_125(Some("create")));
    }

    #[test]
    fn given_volume_flag_when_create_parsed_then_short_and_long_both_work() {
        // Arrange
        let long = ["ward", "create", "alpine", "--volume", "/a:/b"];
        let short = ["ward", "create", "alpine", "-v", "/a:/b"];

        // Act
        let parsed = [long, short].map(Cli::try_parse_from);

        // Assert
        for cli in parsed {
            match cli.expect("parse").command {
                Commands::Create { opts, .. } => assert_eq!(opts.volume, ["/a:/b"]),
                other => panic!("expected Create, got {other:?}"),
            }
        }
    }

    #[test]
    fn given_create_with_trailing_command_when_parsed_then_command_is_kept() {
        // Arrange
        let args = ["ward", "create", "alpine", "echo", "-n", "hi"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Create { image, command, .. } => {
                assert_eq!(
                    (image.as_str(), command),
                    ("alpine", vec!["echo".into(), "-n".into(), "hi".into()])
                );
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn given_docker_sizes_when_create_request_built_then_limits_are_converted() {
        // Arrange
        let cli = Cli::try_parse_from(["ward", "create", "alpine", "-m", "2g", "--cpus", "1.5"])
            .expect("parse");
        let Commands::Create { image, opts, .. } = cli.command else {
            panic!("expected Create");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        let limits = request.resources.expect("resources");
        assert_eq!((limits.memory_mb, limits.cpus), (2048, 2));
    }

    #[test]
    fn given_ro_suffix_when_parse_mount_then_readonly() {
        assert!(parse_mount_spec("/tmp/a:/data:ro").unwrap().readonly);
    }

    #[test]
    fn given_malformed_spec_when_parse_mount_then_error() {
        assert!(parse_mount_spec("/tmp/a").is_err());
        assert!(parse_mount_spec("/tmp/a:/data:wat").is_err());
        assert!(parse_mount_spec("a:b:ro:extra").is_err());
    }

    #[test]
    fn given_from_snapshot_flag_without_source_sandbox_when_parsed_then_clap_error() {
        // Arrange
        let args = [
            "ward",
            "create",
            "alpine",
            "--from-snapshot",
            "some-snap-id",
        ];

        // Act
        let result = Cli::try_parse_from(args);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn given_ps_with_all_flag_when_parsed_then_all_is_set() {
        // Arrange
        let args = ["ward", "ps", "-a"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        assert!(matches!(cli.command, Commands::Ps { all: true, .. }));
    }

    #[test]
    fn given_ps_with_quiet_and_format_when_parsed_then_both_are_set() {
        // Arrange
        let args = ["ward", "ps", "-q", "--format", "json"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        assert!(matches!(
            cli.command,
            Commands::Ps { quiet: true, format: Some(f), .. } if f == "json"
        ));
    }

    #[test]
    fn given_kill_with_pid_flag_and_two_refs_when_parsed_then_both_refs_kept() {
        // Arrange
        let args = ["ward", "kill", "--pid", "abc", "web1", "web2"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Kill { refs, pid, .. } => assert_eq!(
                (refs, pid.as_str()),
                (vec!["web1".into(), "web2".into()], "abc")
            ),
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn given_kill_with_signal_and_no_pid_when_parsed_then_pid_is_main_process() {
        // Arrange
        let args = ["ward", "kill", "-s", "TERM", "web1"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Kill { refs, pid, signal } => {
                assert_eq!(
                    (refs.as_slice(), pid.as_str(), signal.as_str()),
                    (["web1".to_string()].as_slice(), "0", "TERM")
                );
            }
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn given_rm_alias_with_force_and_two_refs_when_parsed_then_remove_has_both() {
        // Arrange
        let args = ["ward", "rm", "-f", "web1", "web2"];

        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Remove { refs, force } => {
                assert_eq!(refs, ["web1", "web2"]);
                assert!(force);
            }
            other => panic!("expected Remove, got {other:?}"),
        }
    }
}
