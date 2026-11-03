// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward – command-line interface to the Ward daemon.

mod attach;
mod client;
mod commit;
mod cp;
mod create_opts;
mod df_verbose;
mod diff;
mod docker;
mod egress;
mod events;
mod export;
mod history;
mod images;
mod import;
mod inspect;
mod isolation;
mod lifecycle;
mod load;
mod log_output;
mod login;
mod output;
mod pause;
mod ports;
mod prune;
mod prune_filter;
mod ps_filter;
mod pull;
mod resources;
mod rmi;
mod save;
mod secrets;
mod socket;
mod stats;
mod system;
mod tag;
mod template;
mod terminal;
mod tokens;
mod top;
mod unpause;
mod volume;
mod wait;

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

/// Footer for commands whose behavior differs from Docker's.
const DOCKER_DIFFERENCES: &str = "See docs/docker-differences.md or https://github.com/igorjs/ward/blob/main/docs/docker-differences.md";

const DEFAULT_MEMORY: &str = "512m";
const DEFAULT_CPUS: &str = "1";

/// Help for `--time` on `stop` and `restart`.
fn stop_time_help() -> String {
    format!(
        "Seconds to wait for the main process after its stop signal before SIGKILL (default {}, or the sandbox's --stop-timeout; 0 kills at once, -1 waits forever).",
        ward_core::sandbox::STOP_GRACE_SECONDS
    )
}

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
    /// Process limit, as in Docker: 0 or -1 for unlimited. 0 uses the daemon
    /// default, which is unlimited unless the operator sets one.
    #[arg(
        long = "pids-limit",
        default_value_t = 0,
        allow_negative_numbers = true,
        value_name = "N"
    )]
    pids_limit: i64,
    /// User to run as: name|uid[:group|gid]. Defaults to the image's USER.
    #[arg(short = 'u', long, value_name = "NAME|UID[:GROUP|GID]")]
    user: Option<String>,
    /// Publish a TCP port: [HOST_IP:][HOST_PORT:]CONTAINER_PORT[/tcp]. Without
    /// HOST_PORT, or with 0, the daemon picks a free one. Repeatable.
    /// Unlike Docker, HOST_IP defaults to 127.0.0.1, not 0.0.0.0; the port
    /// stays open while the sandbox is exited and closes on stop or rm; TCP only.
    #[arg(
        short = 'p',
        long = "publish",
        value_name = "[HOST_IP:][HOST_PORT:]PORT[/tcp]"
    )]
    publish: Vec<String>,
    /// When to pull the image: "always", "missing" (default), "never", or
    /// "pinned" (like missing, but the reference must carry a @sha256 digest).
    #[arg(long, value_enum, default_value_t = PullArg::Missing, value_name = "POLICY")]
    pull: PullArg,
    /// Remove the sandbox this long after it ends: seconds, or a duration like
    /// 10m or 1h30m. Off unless set here or by the daemon.
    #[arg(long = "exited-ttl", value_name = "DURATION", value_parser = parse_exited_ttl)]
    exited_ttl: Option<u64>,
    #[command(flatten)]
    extras: create_opts::CreateExtras,
    #[command(flatten)]
    isolation: isolation::IsolationFlags,
}

/// Parse `--exited-ttl` into whole seconds, rounding a fraction up.
fn parse_exited_ttl(value: &str) -> Result<u64, String> {
    let invalid = || format!("invalid duration '{value}': use seconds or a value like 10m, 1h30m");
    if let Ok(seconds) = value.parse::<u64>() {
        return Ok(seconds);
    }
    let duration = log_output::parse_duration(value).ok_or_else(invalid)?;
    Ok(duration.as_secs() + u64::from(duration.subsec_nanos() > 0))
}

/// The `--pull` values: Docker's three, plus Ward's `pinned`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PullArg {
    Always,
    Missing,
    Never,
    Pinned,
}

impl From<PullArg> for ward_core::pb::PullPolicy {
    fn from(arg: PullArg) -> Self {
        match arg {
            PullArg::Always => Self::Always,
            PullArg::Missing => Self::Missing,
            PullArg::Never => Self::Never,
            PullArg::Pinned => Self::Pinned,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Create a new sandbox without starting a command, and print its ID.
    Create {
        /// OCI image reference.
        image: String,
        /// Command and arguments the sandbox runs.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
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
        /// Accepted only to explain that secrets need run or exec.
        #[arg(long = "secret", hide = true, value_name = "FILE")]
        secret: Vec<String>,
    },

    /// List all sandboxes.
    List,

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// List running sandboxes, or every sandbox with -a.
    Ps {
        /// Show sandboxes in every state.
        #[arg(short, long)]
        all: bool,
        /// Print only the sandbox IDs.
        #[arg(short, long)]
        quiet: bool,
        /// Go template for each sandbox, or `json`. Fields: .ID .Image
        /// .Command .CreatedAt .RunningFor .State .Status .Reason .Names.
        #[arg(long)]
        format: Option<String>,
        /// Print the full sandbox ID.
        #[arg(long)]
        no_trunc: bool,
        /// Show the N most recently created sandboxes in any state, -1 for all.
        #[arg(
            short = 'n',
            long,
            default_value_t = ps_filter::NO_LIMIT,
            allow_negative_numbers = true,
            value_name = "N"
        )]
        last: i64,
        /// Show the most recently created sandbox in any state.
        #[arg(short = 'l', long)]
        latest: bool,
        /// Only sandboxes matching name=, id= (a prefix) or status=
        /// (created, running, stopping, stopped, exited, failed). Repeatable.
        /// A status filter looks at every state.
        #[arg(short, long)]
        filter: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Print the host address each published port is reachable on.
    ///
    /// With PORT (80 or 80/tcp), prints only the host address for it.
    ///
    /// Unlike Docker, a port without HOST_IP binds 127.0.0.1, not 0.0.0.0. It
    /// stays open while the sandbox is exited and closes on stop or rm. TCP only.
    Port {
        /// Sandbox name, ID or ID prefix.
        reference: String,
        /// Container port, optionally with /tcp.
        port: Option<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Print low level details of one or more sandboxes as a JSON array.
    ///
    /// --format takes a Go template over the JSON, such as
    /// `{{.State.Status}}` or `{{json .Config}}`.
    Inspect {
        /// Go template for each sandbox.
        #[arg(short, long)]
        format: Option<String>,
        /// Kind of object to inspect: container. Ward cannot inspect images yet.
        #[arg(long = "type", value_name = "TYPE")]
        kind: Option<String>,
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        references: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
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
        /// User to run as: name|uid[:group|gid]. Defaults to the sandbox's user.
        #[arg(short = 'u', long, value_name = "NAME|UID[:GROUP|GID]")]
        user: Option<String>,
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
        /// Give the command a secret file at /run/secrets/NAME, read from
        /// FILE on this host. NAME defaults to the base name of FILE.
        #[arg(long, value_name = "[NAME=]FILE")]
        secret: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Create a sandbox, run a command in it and exit with its exit code.
    Run {
        /// OCI image reference.
        image: String,
        /// Command and arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        #[command(flatten)]
        opts: SandboxOpts,
        /// Replace the image entrypoint. An empty value clears it.
        #[arg(long)]
        entrypoint: Option<String>,
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
        #[arg(long)]
        rm: bool,
        /// Print the sandbox ID and leave the command running.
        #[arg(short, long)]
        detach: bool,
        /// Give the command a secret file at /run/secrets/NAME, read from
        /// FILE on this host. NAME defaults to the base name of FILE.
        #[arg(long, value_name = "[NAME=]FILE")]
        secret: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
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
        /// Only output at or after this time: a duration such as 10m or
        /// 1h30m, or an RFC3339 timestamp.
        #[arg(long, conflicts_with = "pid")]
        since: Option<String>,
        /// Prefix every line with its RFC3339 arrival time.
        #[arg(short = 't', long, conflicts_with = "pid")]
        timestamps: bool,
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
        #[arg(short = 't', long = "time", visible_alias = "timeout", allow_hyphen_values = true, help = stop_time_help())]
        time: Option<i32>,
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
        #[arg(short = 't', long = "time", visible_alias = "timeout", allow_hyphen_values = true, help = stop_time_help())]
        time: Option<i32>,
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

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Copy files between a sandbox and the host, as `docker cp` does.
    ///
    /// One of SRC and DEST is `SANDBOX:PATH`. Use `-` for SRC to read a tar
    /// from stdin, or for DEST to write a tar to stdout. A `/.` suffix on a
    /// directory copies its contents. A relative sandbox path starts at the
    /// sandbox's working directory.
    Cp {
        /// A host path, `-` for stdin, or `SANDBOX:PATH`.
        src: String,
        /// A host path, `-` for stdout, or `SANDBOX:PATH`.
        dest: String,
        /// Copy the file a host symlink SRC points to, not the link.
        #[arg(short = 'L', long)]
        follow_link: bool,
        /// Accepted for `docker cp` scripts; ownership is never carried over.
        #[arg(short = 'a', long)]
        archive: bool,
        /// Accepted for `docker cp` scripts; `ward cp` prints no progress.
        #[arg(short = 'q', long)]
        quiet: bool,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// List the processes of a running sandbox, as `docker top` does.
    ///
    /// Options after the sandbox are ps-style: none (as `ps -ef`), aux, axu,
    /// -e, -A, -f, -ef, -Af, -o or -eo with a column list (pid, ppid, uid,
    /// user, comm, args, cmd, time, stime, tty, stat, vsz, rss, pcpu, pmem,
    /// c), -u USER and -p PID. Times are UTC.
    Top {
        /// Sandbox name, ID or ID prefix.
        reference: String,
        /// Options for `ps`.
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        ps_args: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Show the CPU, memory and process use of running sandboxes.
    ///
    /// Refreshes every second until interrupted; --no-stream prints one
    /// reading. With no sandbox named it covers every running sandbox this
    /// user holds a token for. CPU % is of one CPU. Network and block I/O are
    /// not measured and print `--`. --format takes a Go template over
    /// .Container .ID .Name .CPUPerc .MemUsage .MemPerc .NetIO .BlockIO .PIDs,
    /// `table ` prefix and `{{json .}}`.
    Stats {
        /// Show stopped sandboxes too, with `--` for their figures.
        #[arg(short, long)]
        all: bool,
        /// Print the full sandbox ID.
        #[arg(long)]
        no_trunc: bool,
        /// Print one reading instead of streaming.
        #[arg(long)]
        no_stream: bool,
        /// Go template for each row.
        #[arg(long)]
        format: Option<String>,
        /// Sandbox names, IDs or ID prefixes.
        references: Vec<String>,
    },

    /// Block until sandboxes stop, then print their exit codes.
    ///
    /// A sandbox without a command reports 0 once it is stopped.
    Wait {
        /// Sandbox names, IDs or ID prefixes.
        #[arg(required = true)]
        references: Vec<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Stream sandbox events: create, start, die, stop and destroy.
    ///
    /// The daemon answers without a sandbox token, like `ward ps`, and keeps
    /// only the last 1024 events in memory, destroyed sandboxes included and
    /// none across a restart. --since replays from that history. --filter
    /// takes type, event, container or name (a name or ID prefix), image and
    /// label (an attribute, as key or key=value). --format takes a Go
    /// template over .Type .Action .Status .ID .From .Scope .Time .TimeNano
    /// .Actor.ID .Actor.Attributes.<key>, or json.
    Events {
        /// Replay events from this time: a duration, timestamp or Unix seconds.
        #[arg(long)]
        since: Option<String>,
        /// Stop streaming at this time.
        #[arg(long)]
        until: Option<String>,
        /// Only events matching name=value. Repeatable.
        #[arg(short, long)]
        filter: Vec<String>,
        /// Go template for each event.
        #[arg(long)]
        format: Option<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// List the images in the local cache.
    ///
    /// Like `docker images`, it shows REPOSITORY, TAG, IMAGE ID, CREATED and
    /// SIZE. Every cached image is a final image, so -a adds nothing.
    /// --filter takes reference=<pattern> (`*` matches any run of characters)
    /// and dangling=true|false; no cached image is dangling. --format takes a
    /// Go template over .Repository .Tag .ID .Digest .CreatedAt .CreatedSince
    /// .Size, the `table ` prefix, and `{{json .}}`.
    Images {
        /// Show all images. Ward keeps no intermediate images.
        #[arg(short, long)]
        all: bool,
        /// Print only image IDs.
        #[arg(short, long)]
        quiet: bool,
        /// Print the full image ID.
        #[arg(long)]
        no_trunc: bool,
        /// Go template for each image.
        #[arg(long)]
        format: Option<String>,
        /// Only images matching name=value. Repeatable.
        #[arg(short, long)]
        filter: Vec<String>,
        /// Only images of this REPOSITORY[:TAG].
        reference: Option<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Pull an image into the local cache.
    ///
    /// Progress prints like `docker pull`. A pull stops at 2 GiB of layers
    /// and fails instead of evicting images when the cache is full.
    Pull {
        /// Print only the image reference once the pull is done.
        #[arg(short, long)]
        quiet: bool,
        #[command(flatten)]
        flags: pull::Flags,
        /// Image to pull: IMAGE[:TAG|@DIGEST].
        image: String,
    },

    /// Show the paths a sandbox changed against its image, as `docker diff`
    /// does: `A` added, `C` changed, `D` deleted.
    Diff {
        /// Sandbox name, ID or ID prefix.
        reference: String,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Save a sandbox's filesystem as a new local image, as `docker commit`
    /// does, and print the new image id.
    ///
    /// The image takes the command, environment and working directory the
    /// sandbox started with, then CHANGE on top. CHANGE accepts CMD,
    /// ENTRYPOINT, ENV, WORKDIR and USER. A sandbox cannot be paused yet, so
    /// the copy is taken while it runs and a file being written may be copied
    /// part way. An unreadable file fails the commit. Device nodes, pipes and
    /// sockets are not copied; hard links become separate files; setuid and
    /// setgid bits are dropped; extended attributes, sparse holes and
    /// modification times are not kept. A committed image is never evicted
    /// from the cache. A tag already in use moves to the new image, and the
    /// old one stays by its digest.
    Commit {
        /// Author of the commit, e.g. "Ada <ada@example.com>".
        #[arg(short, long)]
        author: Option<String>,
        /// Commit message.
        #[arg(short, long)]
        message: Option<String>,
        /// Apply a Dockerfile instruction to the image config. Repeatable.
        #[arg(short = 'c', long = "change", value_name = "CHANGE")]
        change: Vec<String>,
        /// Pause the sandbox during the commit. Accepted for Docker
        /// compatibility: pausing is not supported yet and the copy is live.
        #[arg(short, long, default_value_t = true, action = clap::ArgAction::Set)]
        pause: bool,
        /// Sandbox name, ID or ID prefix.
        container: String,
        /// Local tag for the new image, REPOSITORY[:TAG].
        repository: Option<String>,
    },

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Remove tags and images from the local cache, as `docker rmi` does.
    ///
    /// A tag that is not the image's last is only untagged. An image a
    /// running sandbox uses is never deleted. -f deletes one that stopped
    /// sandboxes use, or one with several tags given by ID.
    Rmi(rmi::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Write images to a tar archive, as `docker save` does.
    ///
    /// The archive is an OCI image layout that `docker load` and `podman
    /// load` also read. Each image is saved as one flattened layer.
    Save(save::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Load images from a tar archive, as `docker load` does.
    ///
    /// Reads archives from `ward save`, `docker save` and `podman save`.
    Load(load::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Write a sandbox's filesystem to a tar archive, as `docker export` does.
    ///
    /// Works in any state. Owners are written as root.
    Export(export::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Make a one-layer local image from a tar archive, as `docker import`
    /// does, and print its id.
    Import(import::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Store registry credentials in Docker's config.json for pulls.
    ///
    /// The credentials are checked with the daemon first. The file is
    /// `$DOCKER_CONFIG/config.json` or `~/.docker/config.json`, mode 0600,
    /// and the daemon reads it as the user it runs as.
    Login(login::LoginArgs),

    /// Remove stored registry credentials.
    Logout(login::LogoutArgs),

    /// Add a tag to a local image, as `docker tag` does.
    Tag(tag::Args),

    #[command(after_help = DOCKER_DIFFERENCES)]
    /// Show the layers of an image, newest first, as `docker history` does.
    History(history::Args),

    /// Give a sandbox a new name.
    Rename {
        /// Sandbox name, ID or ID prefix.
        reference: String,
        /// The new name.
        new_name: String,
    },

    /// Remove every stopped and exited sandbox.
    Prune,

    /// Remove stopped sandboxes, asking first unless -f is given.
    #[command(name = "container-prune", hide = true)]
    ContainerPrune(prune::ContainerArgs),

    /// Remove images no sandbox uses, asking first unless -f is given.
    #[command(name = "image-prune", hide = true)]
    ImagePrune(prune::ImageArgs),

    /// Freeze the processes of one or more running sandboxes.
    Pause(pause::Args),

    /// Thaw the processes of one or more paused sandboxes.
    Unpause(unpause::Args),

    /// Show the client and daemon versions.
    Version {
        /// Go template over .Client.Version and .Server.Version, .Server.Os,
        /// .Server.Arch and .Server.Backend.
        #[arg(short, long)]
        format: Option<String>,
    },

    /// Daemon-wide commands.
    #[command(subcommand)]
    System(SystemCommands),

    /// Manage sandboxes: ls, ps, create, run, start, stop, restart, rm, kill,
    /// logs, inspect, exec, cp, top, stats, wait, diff, commit, prune, port,
    /// rename, pause and unpause. Each is the top-level command of the same name.
    Container {
        /// Subcommand and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Manage images: ls, pull, rm, tag, history, save, load and import. Each
    /// is the top-level command of the same name (`ls` is `images`, `rm` is
    /// `rmi`).
    Image {
        /// Subcommand and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Volume management subcommands.
    #[command(subcommand)]
    Volume(VolumeCommands),

    /// Egress policy helpers.
    #[command(subcommand)]
    Egress(EgressCommands),

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
enum SystemCommands {
    /// Show daemon information.
    Info,
    /// Count sandboxes and cached images.
    Df {
        /// Show a table per sandbox, image and volume.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Remove stopped sandboxes, and with -a unused images and with
    /// --volumes volumes, asking first unless -f is given.
    Prune(prune::SystemArgs),
}

#[derive(Debug, Subcommand)]
enum EgressCommands {
    /// Print `--allow-domain` flags for the hosts a sandbox was blocked from.
    ///
    /// Ward-only: Docker has no equivalent. Records are kept after the
    /// sandbox exits, until it is removed.
    Suggest {
        /// Sandbox name, ID or ID prefix.
        reference: String,
        /// Add the hosts to the sandbox's allowlist at once, and keep them
        /// across restarts. Needs a sandbox created with `--egress
        /// allowlist`. IP addresses are skipped.
        #[arg(long)]
        apply: bool,
    },
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
    /// List volumes.
    #[command(alias = "ls")]
    List(volume::ListArgs),
    /// Show details of one or more volumes as a JSON array.
    Inspect(volume::InspectArgs),
    /// Remove one or more volumes, by name or ID.
    #[command(alias = "rm")]
    Remove(volume::RemoveArgs),
    /// Remove unused volumes, asking first.
    Prune(prune::VolumeArgs),
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
        pids_max: resources::parse_pids_limit(opts.pids_limit)?,
        timeout_seconds,
        exited_ttl_seconds: opts.exited_ttl.unwrap_or_default(),
    };
    opts.isolation.check()?;
    let mounts = opts
        .volume
        .iter()
        .map(|spec| parse_mount_spec(spec))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let published_ports = ports::parse_all(&opts.publish)?;
    ports::warn_if_exposed(&published_ports);
    let mut request = ward_core::pb::CreateSandboxRequest {
        image,
        mounts,
        published_ports,
        egress: Some(ward_core::pb::EgressPolicy {
            mode: egress_mode as i32,
            domains: opts.allow_domain,
        }),
        resources: Some(resources),
        name: opts.name.unwrap_or_default(),
        user: opts.user.unwrap_or_default(),
        pull_policy: ward_core::pb::PullPolicy::from(opts.pull) as i32,
        ..Default::default()
    };
    opts.extras.apply(&mut request, &opts.env)?;
    Ok(request)
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
    let args: Vec<String> = args.into_iter().collect();
    subcommand_index(&args).map(|index| args[index].clone())
}

fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = 1;
    while let Some(arg) = args.get(index) {
        if arg == "--socket" {
            index += 1;
        } else if !arg.starts_with('-') {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Docker's `container` subcommands and the top-level command each stands for.
const CONTAINER_COMMANDS: [(&str, &str); 24] = [
    ("ls", "ps"),
    ("ps", "ps"),
    ("create", "create"),
    ("run", "run"),
    ("start", "start"),
    ("stop", "stop"),
    ("restart", "restart"),
    ("rm", "rm"),
    ("remove", "rm"),
    ("kill", "kill"),
    ("logs", "logs"),
    ("inspect", "inspect"),
    ("exec", "exec"),
    ("cp", "cp"),
    ("top", "top"),
    ("stats", "stats"),
    ("wait", "wait"),
    ("diff", "diff"),
    ("commit", "commit"),
    ("prune", "container-prune"),
    ("port", "port"),
    ("rename", "rename"),
    ("pause", "pause"),
    ("unpause", "unpause"),
];
/// Docker's `image` subcommands that Ward has, and what they stand for.
const IMAGE_COMMANDS: [(&str, &str); 11] = [
    ("prune", "image-prune"),
    ("ls", "images"),
    ("list", "images"),
    ("pull", "pull"),
    ("rm", "rmi"),
    ("remove", "rmi"),
    ("tag", "tag"),
    ("history", "history"),
    ("save", "save"),
    ("load", "load"),
    ("import", "import"),
];

/// Rewrite `ward container ls -a` to `ward ps -a`, and the `image` group the
/// same way, so both spellings share one definition and one help text.
fn expand_groups(mut args: Vec<String>) -> Vec<String> {
    let Some(group) = subcommand_index(&args) else {
        return args;
    };
    let table: &[(&str, &str)] = match args[group].as_str() {
        "container" => &CONTAINER_COMMANDS,
        "image" => &IMAGE_COMMANDS,
        _ => return args,
    };
    let target = args
        .get(group + 1)
        .and_then(|sub| table.iter().find(|(name, _)| name == sub))
        .map(|(_, command)| (*command).to_string());
    if let Some(command) = target {
        args.splice(group..=group + 1, [command]);
    }
    args
}

/// `run` and `exec` report their own failures as 125, so that scripts can tell
/// them apart from the exit code of the command they ran.
fn reports_cli_failure_as_125(subcommand: Option<&str>) -> bool {
    matches!(subcommand, Some("run" | "exec"))
}

#[tokio::main]
async fn main() {
    let args = expand_groups(std::env::args().collect());
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr()
                && reports_cli_failure_as_125(subcommand_name(args.clone()).as_deref())
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
        match client::daemon_error_line(&e) {
            Some(line) => eprintln!("{line}"),
            None => eprintln!("Error: {e:?}"),
        }
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
            secret,
        } => {
            secrets::refuse_on_create(&secret)?;
            let comms_mode_pb = match comms_mode.as_str() {
                "deny" => ward_core::pb::CommunicationMode::Deny,
                "group" => ward_core::pb::CommunicationMode::Group,
                other => anyhow::bail!("--comms-mode must be 'deny' or 'group', got '{other}'"),
            };
            let hooks = opts.extras.hooks(opts.name.as_deref());
            let mut req = create_request(image, opts, timeout)?;
            req.command = command;
            req.comms = Some(ward_core::pb::CommunicationPolicy {
                mode: comms_mode_pb as i32,
                group: comms_group,
            });
            req.from_snapshot = from_snapshot.clone().unwrap_or_default();
            let mut c = client::connect(&socket_path).await?;
            hooks.begin(&mut c, &token_store).await?;

            let created = async {
                if let (Some(_), Some(source_sandbox)) = (&from_snapshot, &source_sandbox) {
                    // Restoring from a snapshot requires the source sandbox's
                    // token to authorize the daemon's from_snapshot gate.
                    let source_token = tokens::load(&token_store, source_sandbox)?;
                    let mut request = tonic::Request::new(req);
                    tokens::attach_token(&mut request, source_token)?;
                    Ok(c.create_sandbox(request).await?.into_inner())
                } else {
                    Ok(c.create_sandbox(req).await?.into_inner())
                }
            }
            .await;
            let resp = hooks.finish(created)?;

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

        Commands::Ps {
            all,
            quiet,
            format,
            no_trunc,
            last,
            latest,
            filter,
        } => {
            let mut c = client::connect(&socket_path).await?;
            let options = inspect::PsOptions {
                all,
                quiet,
                json,
                format,
                no_pretty,
                no_trunc,
                filters: ps_filter::Filters::parse(&filter)?,
                limit: ps_filter::limit(last, latest),
            };
            inspect::ps(&mut c, options).await?;
        }

        Commands::Port { reference, port } => {
            ports::run(
                &socket_path,
                &token_store,
                &reference,
                port.as_deref(),
                json,
            )
            .await?;
        }

        Commands::Inspect {
            format,
            kind,
            references,
        } => {
            inspect::check_type(kind.as_deref())?;
            let mut c = client::connect(&socket_path).await?;
            inspect::inspect(&mut c, &token_store, &references, format.as_deref()).await?;
        }

        Commands::Exec {
            id,
            command,
            workdir,
            user,
            env,
            interactive,
            tty,
            detach,
            secret,
        } => {
            let secrets = secrets::load_all(&secret)?;
            if !detach {
                let spec = docker::ExecSpec {
                    secrets,
                    command,
                    env: parse_env(&env),
                    workdir: workdir.unwrap_or_default(),
                    user: user.unwrap_or_default(),
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
                user: user.unwrap_or_default(),
                secrets,
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
            entrypoint,
            workdir,
            interactive,
            tty,
            rm,
            detach,
            secret,
        } => {
            opts.extras.check_rm(rm)?;
            let hooks = opts.extras.hooks(opts.name.as_deref());
            let mut request = create_request(image, opts, DAEMON_DEFAULT_TIMEOUT_SECONDS)?;
            request.secrets = secrets::load_all(&secret)?;
            request.command = command;
            request.use_image_config = true;
            request.entrypoint = entrypoint;
            request.require_command = true;
            request.tty = tty;
            request.workdir = workdir.unwrap_or_default();
            // A detached run has no client to remove it, so the daemon does.
            request.auto_remove = rm && detach;
            let flags = docker::RunFlags {
                interactive,
                tty,
                rm,
                detach,
                hooks,
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
            since,
            timestamps,
        } => {
            let options = log_output::LogOptions {
                tail: docker::parse_tail(&tail)?,
                since: since
                    .map(|value| log_output::parse_since(&value, chrono::Utc::now()))
                    .transpose()?,
                timestamps,
            };
            let code = docker::logs(&socket_path, &token_store, &id, follow, options).await?;
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

        Commands::Stop { refs, time } => {
            let mut c = client::connect(&socket_path).await?;
            let action = lifecycle::Action::Stop;
            lifecycle::lifecycle(&mut c, &token_store, action, &refs, time, json).await?;
        }

        Commands::Start { refs } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::lifecycle(
                &mut c,
                &token_store,
                lifecycle::Action::Start,
                &refs,
                None,
                json,
            )
            .await?;
        }

        Commands::Restart { refs, time } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::lifecycle(
                &mut c,
                &token_store,
                lifecycle::Action::Restart,
                &refs,
                time,
                json,
            )
            .await?;
        }

        Commands::Remove { refs, force } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::remove(&mut c, &token_store, &refs, force, json).await?;
        }

        Commands::Cp {
            src,
            dest,
            follow_link,
            ..
        } => {
            cp::run(&socket_path, &token_store, &src, &dest, follow_link).await?;
        }

        Commands::Egress(EgressCommands::Suggest { reference, apply }) => {
            egress::suggest(&socket_path, &token_store, &reference, apply, json).await?;
        }

        Commands::Top { reference, ps_args } => {
            top::run(&socket_path, &token_store, &reference, ps_args, json).await?;
        }

        Commands::Stats {
            all,
            no_trunc,
            no_stream,
            format,
            references,
        } => {
            let options = stats::Options {
                no_stream,
                format,
                json,
                all,
                no_trunc,
            };
            stats::run(&socket_path, &token_store, &references, options).await?;
        }

        Commands::Wait { references } => {
            wait::run(&socket_path, &token_store, &references, json).await?;
        }

        Commands::Events {
            since,
            until,
            filter,
            format,
        } => {
            let options = events::Options {
                since,
                until,
                filters: filter,
                format,
                json,
            };
            events::run(&socket_path, options).await?;
        }

        Commands::Images {
            quiet,
            no_trunc,
            format,
            filter,
            reference,
            ..
        } => {
            let options = images::Options {
                quiet,
                no_trunc,
                format,
                filters: filter,
                reference,
                json,
            };
            images::run(&socket_path, options).await?;
        }

        Commands::Pull {
            quiet,
            flags,
            image,
        } => {
            flags.check()?;
            pull::run(&socket_path, &image, quiet).await?;
        }

        Commands::Diff { reference } => {
            diff::run(&socket_path, &token_store, &reference, json).await?;
        }

        Commands::Commit {
            author,
            message,
            change,
            pause,
            container,
            repository,
        } => {
            let options = commit::Options {
                author: author.unwrap_or_default(),
                message: message.unwrap_or_default(),
                changes: change,
                pause,
                reference: repository.unwrap_or_default(),
            };
            commit::run(&socket_path, &token_store, &container, options).await?;
        }

        Commands::Rmi(args) => rmi::run(&socket_path, args).await?,

        Commands::Save(args) => save::run(&socket_path, args).await?,

        Commands::Load(args) => load::run(&socket_path, args).await?,

        Commands::Export(args) => export::run(&socket_path, &token_store, args).await?,

        Commands::Import(args) => import::run(&socket_path, args).await?,

        Commands::Login(args) => login::login(&socket_path, args).await?,

        Commands::Logout(args) => login::logout(&args)?,

        Commands::Tag(args) => tag::run(&socket_path, args).await?,

        Commands::History(args) => history::run(&socket_path, args, json).await?,

        Commands::Rename {
            reference,
            new_name,
        } => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::rename(&mut c, &token_store, &reference, &new_name, json).await?;
        }

        Commands::Pause(args) => pause::run(&socket_path, &token_store, args, json).await?,

        Commands::Unpause(args) => unpause::run(&socket_path, &token_store, args, json).await?,

        Commands::Prune => {
            let mut c = client::connect(&socket_path).await?;
            lifecycle::prune(&mut c, &token_store, json).await?;
        }

        Commands::ContainerPrune(args) => {
            prune::container(&socket_path, &token_store, args, json).await?;
        }

        Commands::ImagePrune(args) => prune::image(&socket_path, args, json).await?,

        Commands::System(SystemCommands::Prune(args)) => {
            prune::system(&socket_path, &token_store, args, json).await?;
        }

        Commands::Version { format } => {
            system::version(&socket_path, format.as_deref(), json).await?;
        }

        Commands::System(SystemCommands::Df { verbose: true }) => {
            df_verbose::run(&socket_path).await?;
        }

        Commands::System(SystemCommands::Df { verbose: false }) => {
            system::df(&socket_path, json, no_pretty).await?;
        }

        Commands::Container { .. } | Commands::Image { .. } => {
            anyhow::bail!(
                "missing or unknown subcommand; see 'ward container --help' or 'ward image --help'"
            );
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
            VolumeCommands::List(args) => volume::list(&socket_path, args, json, no_pretty).await?,
            VolumeCommands::Inspect(args) => volume::inspect(&socket_path, args).await?,
            VolumeCommands::Prune(args) => prune::volume(&socket_path, args, json).await?,
            VolumeCommands::Remove(args) => volume::remove(&socket_path, args, json).await?,
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

        Commands::Info | Commands::System(SystemCommands::Info) => {
            system::info(&socket_path, json).await?;
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
    use ward_core::pb::PullPolicy;

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

    #[rstest::rstest]
    #[case::container_ls(&["ward", "container", "ls", "-a"], &["ward", "ps", "-a"])]
    #[case::global_flags(&["ward", "--socket", "s", "--json", "container", "rm", "-f", "x"], &["ward", "--socket", "s", "--json", "rm", "-f", "x"])]
    #[case::container_remove(&["ward", "container", "remove", "x"], &["ward", "rm", "x"])]
    #[case::image_ls(&["ward", "image", "ls"], &["ward", "images"])]
    #[case::image_pull(&["ward", "image", "pull", "alpine"], &["ward", "pull", "alpine"])]
    #[case::unknown_sub_is_left_alone(&["ward", "container", "nope"], &["ward", "container", "nope"])]
    #[case::other_command(&["ward", "ps", "container"], &["ward", "ps", "container"])]
    fn given_group_command_when_expanded_then_it_names_the_top_level_command(
        #[case] args: &[&str],
        #[case] expected: &[&str],
    ) {
        let args = args.iter().map(ToString::to_string).collect();

        assert_eq!(expand_groups(args), expected);
    }

    #[test]
    fn given_every_group_target_when_parsed_then_the_command_exists() {
        use clap::CommandFactory;
        let cli = Cli::command();

        for (_, target) in CONTAINER_COMMANDS.iter().chain(&IMAGE_COMMANDS) {
            assert!(
                cli.find_subcommand(target).is_some(),
                "no top-level {target}"
            );
        }
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

    #[rstest::rstest]
    #[case::unset(&[], 0)]
    #[case::count(&["--pids-limit", "256"], 256)]
    #[case::docker_unlimited(&["--pids-limit", "-1"], 0)]
    #[case::equals_form(&["--pids-limit=64"], 64)]
    fn given_pids_limit_flag_when_request_built_then_pids_max_follows(
        #[case] flags: &[&str],
        #[case] want: u32,
    ) {
        // Arrange
        let args = ["ward", "create", "alpine"]
            .into_iter()
            .chain(flags.iter().copied());
        let cli = Cli::try_parse_from(args).expect("parse");
        let Commands::Create { image, opts, .. } = cli.command else {
            panic!("expected Create");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        assert_eq!(request.resources.expect("resources").pids_max, want);
    }

    #[test]
    fn given_pids_limit_flag_on_run_when_parsed_then_it_is_accepted() {
        // Arrange / Act
        let cli = Cli::try_parse_from(["ward", "run", "--pids-limit", "32", "alpine", "true"]);

        // Assert
        assert!(cli.is_ok());
    }

    #[test]
    fn given_create_when_request_built_then_image_config_is_not_used() {
        // Arrange
        let cli = Cli::try_parse_from(["ward", "create", "alpine"]).expect("parse");
        let Commands::Create { image, opts, .. } = cli.command else {
            panic!("expected Create");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        assert!(!request.use_image_config);
        assert_eq!(request.entrypoint, None);
    }

    #[rstest::rstest]
    #[case::short(&["ward", "run", "-u", "1000:1000", "alpine"], "1000:1000")]
    #[case::long(&["ward", "create", "--user", "app", "alpine"], "app")]
    #[case::absent(&["ward", "run", "alpine"], "")]
    fn given_user_flag_when_create_request_built_then_user_is_set(
        #[case] args: &[&str],
        #[case] expected: &str,
    ) {
        // Arrange
        let cli = Cli::try_parse_from(args).expect("parse");
        let (Commands::Create { image, opts, .. } | Commands::Run { image, opts, .. }) =
            cli.command
        else {
            panic!("expected Create or Run");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        assert_eq!(request.user, expected);
    }

    #[test]
    fn given_repeated_publish_flags_when_create_request_built_then_each_becomes_a_mapping() {
        // Arrange
        let cli = Cli::try_parse_from([
            "ward",
            "run",
            "-p",
            "0:80",
            "--publish",
            "127.0.0.1:8443:443",
            "alpine",
        ])
        .expect("parse");
        let Commands::Run { image, opts, .. } = cli.command else {
            panic!("expected Run");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        let ports: Vec<_> = request
            .published_ports
            .iter()
            .map(|m| (m.host_ip.as_str(), m.host_port, m.container_port))
            .collect();
        assert_eq!(ports, vec![("", 0, 80), ("127.0.0.1", 8443, 443)]);
    }

    #[rstest::rstest]
    #[case::default(&[], PullPolicy::Missing)]
    #[case::always(&["--pull=always"], PullPolicy::Always)]
    #[case::missing(&["--pull", "missing"], PullPolicy::Missing)]
    #[case::never(&["--pull=never"], PullPolicy::Never)]
    #[case::pinned(&["--pull=pinned"], PullPolicy::Pinned)]
    fn given_pull_flag_when_create_request_built_then_it_carries_the_policy(
        #[case] flags: &[&str],
        #[case] expected: PullPolicy,
    ) {
        // Arrange
        let mut args = vec!["ward", "create"];
        args.extend(flags);
        args.push("alpine");
        let cli = Cli::try_parse_from(args).expect("parse");
        let Commands::Create { image, opts, .. } = cli.command else {
            panic!("expected Create");
        };

        // Act
        let request = create_request(image, opts, 0).expect("request");

        // Assert
        assert_eq!(request.pull_policy, expected as i32);
    }

    #[test]
    fn given_unknown_pull_value_when_parsed_then_it_is_rejected() {
        // Act
        let result = Cli::try_parse_from(["ward", "run", "--pull=sometimes", "alpine"]);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn given_port_command_when_parsed_then_sandbox_and_port_are_kept() {
        // Act
        let cli = Cli::try_parse_from(["ward", "port", "web", "80/tcp"]).expect("parse");

        // Assert
        match cli.command {
            Commands::Port { reference, port } => {
                assert_eq!(
                    (reference.as_str(), port.as_deref()),
                    ("web", Some("80/tcp"))
                );
            }
            other => panic!("expected Port, got {other:?}"),
        }
    }

    #[test]
    fn given_exec_with_user_flag_when_parsed_then_user_is_kept() {
        // Act
        let cli = Cli::try_parse_from(["ward", "exec", "-u", "1000", "sb", "id"]).expect("parse");

        // Assert
        match cli.command {
            Commands::Exec { user, command, .. } => {
                assert_eq!(
                    (user.as_deref(), command),
                    (Some("1000"), vec!["id".into()])
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn given_create_with_entrypoint_flag_when_parsed_then_clap_error() {
        assert!(Cli::try_parse_from(["ward", "create", "--entrypoint", "/x", "alpine"]).is_err());
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
    fn given_commit_with_docker_flags_when_parsed_then_it_holds_them() {
        // Arrange / Act
        let cli = Cli::try_parse_from([
            "ward", "commit", "-a", "Ada", "-m", "msg", "-c", "ENV A=1", "-c", "CMD sh",
            "-p=false", "web1", "mine:v1",
        ])
        .expect("parse");

        // Assert
        match cli.command {
            Commands::Commit {
                author,
                message,
                change,
                pause,
                container,
                repository,
            } => {
                assert_eq!(author.as_deref(), Some("Ada"));
                assert_eq!(message.as_deref(), Some("msg"));
                assert_eq!(change, ["ENV A=1", "CMD sh"]);
                assert!(!pause);
                assert_eq!(container, "web1");
                assert_eq!(repository.as_deref(), Some("mine:v1"));
            }
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    #[test]
    fn given_commit_without_flags_when_parsed_then_it_pauses_and_has_no_tag() {
        // Arrange / Act
        let cli = Cli::try_parse_from(["ward", "commit", "web1"]).expect("parse");

        // Assert
        match cli.command {
            Commands::Commit {
                pause, repository, ..
            } => {
                assert!(pause);
                assert_eq!(repository, None);
            }
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    #[test]
    fn given_diff_with_a_sandbox_when_parsed_then_it_holds_the_reference() {
        // Arrange / Act
        let cli = Cli::try_parse_from(["ward", "diff", "web1"]).expect("parse");

        // Assert
        match cli.command {
            Commands::Diff { reference } => assert_eq!(reference, "web1"),
            other => panic!("expected Diff, got {other:?}"),
        }
    }

    #[rstest::rstest]
    #[case::none(&["ward", "top", "web1"], &[])]
    #[case::ef(&["ward", "top", "web1", "-ef"], &["-ef"])]
    #[case::aux(&["ward", "top", "web1", "aux"], &["aux"])]
    fn given_top_with_ps_options_when_parsed_then_they_follow_the_sandbox(
        #[case] args: &[&str],
        #[case] expected: &[&str],
    ) {
        // Arrange / Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Top { reference, ps_args } => {
                assert_eq!(reference, "web1");
                assert_eq!(ps_args, expected);
            }
            other => panic!("expected Top, got {other:?}"),
        }
    }

    #[test]
    fn given_egress_suggest_with_apply_when_parsed_then_flag_is_set() {
        // Arrange / Act
        let cli =
            Cli::try_parse_from(["ward", "egress", "suggest", "web1", "--apply"]).expect("parse");

        // Assert
        match cli.command {
            Commands::Egress(EgressCommands::Suggest { reference, apply }) => {
                assert_eq!(reference, "web1");
                assert!(apply);
            }
            other => panic!("expected Egress Suggest, got {other:?}"),
        }
    }

    #[rstest::rstest]
    #[case::seconds("90", Some(90))]
    #[case::zero("0", Some(0))]
    #[case::minutes("10m", Some(600))]
    #[case::compound("1h30m", Some(5400))]
    #[case::fraction_rounds_up("1.5s", Some(2))]
    #[case::unknown_unit("3d", None)]
    #[case::words("soon", None)]
    #[case::empty("", None)]
    fn given_exited_ttl_when_parsed_then_it_is_whole_seconds_or_rejected(
        #[case] raw: &str,
        #[case] want: Option<u64>,
    ) {
        // Act
        let parsed = parse_exited_ttl(raw);

        // Assert
        assert_eq!(parsed.ok(), want);
    }

    #[test]
    fn given_exited_ttl_flag_when_create_request_built_then_resources_carry_it() {
        // Arrange
        let cli =
            Cli::try_parse_from(["ward", "create", "--exited-ttl", "2m", "alpine"]).expect("parse");
        let Commands::Create { image, opts, .. } = cli.command else {
            panic!("expected Create");
        };

        // Act
        let req = create_request(image, opts, 0).expect("request");

        // Assert
        assert_eq!(req.resources.expect("resources").exited_ttl_seconds, 120);
    }

    #[rstest::rstest]
    #[case::absent(&["ward", "stop", "web1"], None)]
    #[case::zero(&["ward", "stop", "-t", "0", "web1"], Some(0))]
    #[case::forever(&["ward", "stop", "--time", "-1", "web1"], Some(-1))]
    #[case::timeout_alias(&["ward", "stop", "--timeout", "7", "web1"], Some(7))]
    fn given_stop_time_when_parsed_then_grace_is_none_or_the_number(
        #[case] args: &[&str],
        #[case] want: Option<i32>,
    ) {
        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Stop { time, .. } => assert_eq!(time, want),
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[rstest::rstest]
    #[case::short(&["ward", "restart", "-t", "5", "web1"])]
    #[case::timeout_alias(&["ward", "restart", "--timeout", "5", "web1"])]
    fn given_restart_time_when_parsed_then_it_carries_the_grace(#[case] args: &[&str]) {
        // Act
        let cli = Cli::try_parse_from(args).expect("parse");

        // Assert
        match cli.command {
            Commands::Restart { time, .. } => assert_eq!(time, Some(5)),
            other => panic!("expected Restart, got {other:?}"),
        }
    }

    #[test]
    fn given_stop_time_help_when_built_then_it_names_the_core_default_grace() {
        // Act
        let help = stop_time_help();

        // Assert
        let default = format!("default {}", ward_core::sandbox::STOP_GRACE_SECONDS);
        assert!(help.contains(&default), "got: {help}");
    }

    #[test]
    fn given_rm_with_detach_when_parsed_then_both_flags_are_accepted() {
        // Act
        let cli = Cli::try_parse_from(["ward", "run", "-d", "--rm", "alpine", "true"]);

        // Assert
        assert!(cli.is_ok());
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
