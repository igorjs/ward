// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-mcp: Model Context Protocol server exposing ward sandboxes to LLM agents.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout per the MCP transport spec. Boots
//! an in-process `ward-runtime` so the agent doesn't need a separate
//! `wardd` running. Per ADR-016, an MCP server is a per-process owner of
//! its sandboxes, so embedded mode is the natural fit.
//!
//! Supported MCP methods:
//!   - `initialize`            -- handshake + capability negotiation
//!   - `tools/list`            -- enumerate available tools
//!   - `tools/call`            -- invoke one of the tools
//!   - `notifications/initialized` -- accepted, no-op
//!
//! Tools exposed:
//!   - `ward_create_sandbox`   -- boot a microVM from an OCI image
//!   - `ward_list_sandboxes`   -- list current sandboxes
//!   - `ward_exec`             -- run a command (synchronous capture)
//!   - `ward_remove_sandbox`   -- tear a sandbox down
//!   - `ward_run`              -- exec + collect output synchronously
//!   - `ward_write_stdin`      -- send bytes to a running process
//!   - `ward_kill_process`     -- terminate a running process
//!   - `ward_create_snapshot`  -- snapshot a sandbox
//!   - `ward_restore_snapshot` -- restore a sandbox from snapshot
//!   - `ward_list_snapshots`   -- list snapshots for a sandbox
//!   - `ward_create_volume`    -- allocate a persistent volume
//!   - `ward_list_volumes`     -- list volumes
//!   - `ward_remove_volume`    -- delete a volume
//!
//! See `docs/adr/016-embedded-mode-microvms.md`.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use ward_core::pb;
use ward_core::protocol::StreamEventKind;
use ward_runtime::Runtime;

mod rpc;

use rpc::{Error as RpcError, Request, Response};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "ward-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default timeout for `ward_run` output collection: 30 seconds.
const RUN_TIMEOUT_SECS: u64 = 30;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // MCP servers are spawned by clients (Claude / Cursor / ...) which
    // own the stdio pair. Logs MUST go to stderr -- stdout is the wire
    // protocol. tracing_subscriber defaults to stderr; lock it in.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::io::stdin().is_terminal() {
        // The MCP transport is line-delimited JSON over stdio. A human
        // running `ward-mcp` interactively almost certainly meant to do
        // something else (e.g. `ward` CLI) -- print a hint instead of
        // hanging.
        eprintln!(
            "ward-mcp speaks MCP JSON-RPC over stdio. \
             Spawn it from an MCP-aware client (Claude / Cursor / Codex) \
             rather than running it directly."
        );
    }

    let data_dir = resolve_data_dir()?;
    tracing::info!(data_dir = %data_dir.display(), "starting ward-mcp");
    let runtime = Runtime::builder()
        .data_dir(&data_dir)
        .build()
        .await
        .map_err(|e| format!("init ward-runtime: {e}"))?;

    let server = Server { runtime };
    server.serve_stdio().await
}

fn resolve_data_dir() -> Result<PathBuf, std::io::Error> {
    if let Ok(s) = std::env::var("WARD_DATA_DIR") {
        return Ok(PathBuf::from(s));
    }
    let home = std::env::var("HOME").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "HOME is not set; cannot resolve default ward data dir (set WARD_DATA_DIR)",
        )
    })?;
    Ok(PathBuf::from(home).join(".ward").join("data"))
}

struct Server {
    runtime: Runtime,
}

impl Server {
    async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut lines = BufReader::new(stdin).lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let response = match serde_json::from_str::<Request>(&line) {
                Ok(req) => self.handle(req).await,
                Err(e) => Some(Response::error(
                    None,
                    RpcError::parse_error(format!("invalid JSON-RPC frame: {e}")),
                )),
            };

            // Notifications (no `id`) produce no response -- skip the write.
            let Some(response) = response else { continue };

            let mut payload = serde_json::to_vec(&response)?;
            payload.push(b'\n');
            stdout.write_all(&payload).await?;
            stdout.flush().await?;
        }

        Ok(())
    }

    /// Returns `None` for notifications (no response expected), `Some` for
    /// regular requests.
    async fn handle(&self, req: Request) -> Option<Response> {
        // Notifications have no id and never get a response per JSON-RPC
        // 2.0 §4.1. MCP uses `notifications/initialized` as a one-way
        // confirmation; ignore it (but don't crash).
        if req.id.is_none() {
            tracing::debug!(method = %req.method, "received notification");
            return None;
        }

        let id = req.id.clone();
        let result = match req.method.as_str() {
            "initialize" => self.handle_initialize().await,
            "tools/list" => self.handle_tools_list().await,
            "tools/call" => self.handle_tools_call(req.params).await,
            "ping" => Ok(serde_json::json!({})),
            other => Err(RpcError::method_not_found(format!(
                "method not implemented: {other}"
            ))),
        };

        Some(match result {
            Ok(value) => Response::ok(id, value),
            Err(e) => Response::error(id, e),
        })
    }

    async fn handle_initialize(&self) -> Result<serde_json::Value, RpcError> {
        Ok(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION,
            }
        }))
    }

    async fn handle_tools_list(&self) -> Result<serde_json::Value, RpcError> {
        Ok(serde_json::json!({ "tools": tools_descriptors() }))
    }

    async fn handle_tools_call(
        &self,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct CallParams {
            name: String,
            #[serde(default)]
            arguments: serde_json::Value,
        }
        let call: CallParams = serde_json::from_value(params.unwrap_or(serde_json::Value::Null))
            .map_err(|e| RpcError::invalid_params(format!("tools/call params: {e}")))?;

        let content = match call.name.as_str() {
            "ward_create_sandbox" => self.tool_create_sandbox(call.arguments).await?,
            "ward_list_sandboxes" => self.tool_list_sandboxes().await?,
            "ward_exec" => self.tool_exec(call.arguments).await?,
            "ward_remove_sandbox" => self.tool_remove_sandbox(call.arguments).await?,
            "ward_run" => self.tool_run(call.arguments).await?,
            "ward_write_stdin" => self.tool_write_stdin(call.arguments).await?,
            "ward_kill_process" => self.tool_kill_process(call.arguments).await?,
            "ward_create_snapshot" => self.tool_create_snapshot(call.arguments).await?,
            "ward_restore_snapshot" => self.tool_restore_snapshot(call.arguments).await?,
            "ward_list_snapshots" => self.tool_list_snapshots(call.arguments).await?,
            "ward_create_volume" => self.tool_create_volume(call.arguments).await?,
            "ward_list_volumes" => self.tool_list_volumes().await?,
            "ward_remove_volume" => self.tool_remove_volume(call.arguments).await?,
            other => {
                return Err(RpcError::invalid_params(format!("unknown tool: {other}")));
            }
        };

        Ok(serde_json::json!({
            "content": [{ "type": "text", "text": content }],
            "isError": false,
        }))
    }

    // ── Tool implementations ─────────────────────────────────────────────

    async fn tool_create_sandbox(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            image: String,
            #[serde(default = "default_cpus")]
            cpus: u32,
            #[serde(default = "default_memory")]
            memory_mb: u32,
            #[serde(default)]
            env: HashMap<String, String>,
        }
        fn default_cpus() -> u32 {
            1
        }
        fn default_memory() -> u32 {
            512
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_create_sandbox: {e}")))?;

        let req = pb::CreateSandboxRequest {
            image: args.image.clone(),
            resources: Some(pb::ResourceLimits {
                cpus: args.cpus,
                memory_mb: args.memory_mb,
                pids_max: 0,
                timeout_seconds: 0,
            }),
            env: args.env,
            comms: Some(pb::CommunicationPolicy {
                mode: pb::CommunicationMode::Deny as i32,
                group: String::new(),
            }),
            egress: Some(pb::EgressPolicy {
                mode: pb::EgressMode::Deny as i32,
                domains: Vec::new(),
            }),
            mounts: Vec::new(),
            volume_ids: Vec::new(),
            from_snapshot: String::new(),
        };
        let info = self
            .runtime
            .sandbox_manager()
            .create(req)
            .await
            .map_err(|e| RpcError::internal(format!("create_sandbox: {e}")))?;
        Ok(format!("sandbox {} created from {}", info.id, info.image))
    }

    async fn tool_list_sandboxes(&self) -> Result<String, RpcError> {
        let sandboxes = self
            .runtime
            .sandbox_manager()
            .list()
            .await
            .map_err(|e| RpcError::internal(format!("list: {e}")))?;
        if sandboxes.is_empty() {
            return Ok("(no sandboxes)".into());
        }
        let mut lines = Vec::with_capacity(sandboxes.len());
        for s in sandboxes {
            let status = pb::SandboxStatus::try_from(s.status)
                .map(|st| format!("{st:?}"))
                .unwrap_or_else(|_| "unknown".to_string());
            lines.push(format!("{}\t{}\t{}", s.id, s.image, status));
        }
        Ok(lines.join("\n"))
    }

    async fn tool_exec(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            command: Vec<String>,
            #[serde(default)]
            working_dir: Option<String>,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_exec: {e}")))?;

        let req = pb::ExecRequest {
            sandbox_id: args.sandbox_id.clone(),
            command: args.command,
            working_dir: args.working_dir.unwrap_or_default(),
            env: HashMap::new(),
        };
        let info = self
            .runtime
            .sandbox_manager()
            .exec(req)
            .await
            .map_err(|e| RpcError::internal(format!("exec: {e}")))?;
        Ok(format!(
            "started pid {} in sandbox {}",
            info.pid, args.sandbox_id
        ))
    }

    async fn tool_remove_sandbox(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_remove_sandbox: {e}")))?;
        self.runtime
            .sandbox_manager()
            .remove(&args.id)
            .await
            .map_err(|e| RpcError::internal(format!("remove: {e}")))?;
        Ok(format!("sandbox {} removed", args.id))
    }

    /// Run a command inside a sandbox and collect stdout/stderr synchronously.
    ///
    /// Calls exec then drains stream_output with a 30 s timeout. Returns a
    /// JSON object with stdout, stderr, exit_code, and duration_ms so the
    /// calling agent has structured output without parsing raw text.
    async fn tool_run(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            command: Vec<String>,
            #[serde(default)]
            working_dir: Option<String>,
            /// Override the default 30 s output-collection timeout.
            #[serde(default)]
            timeout_secs: Option<u64>,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_run: {e}")))?;

        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(RUN_TIMEOUT_SECS));

        let exec_req = pb::ExecRequest {
            sandbox_id: args.sandbox_id.clone(),
            command: args.command,
            working_dir: args.working_dir.unwrap_or_default(),
            env: HashMap::new(),
        };
        let mgr = self.runtime.sandbox_manager();
        let proc_info = mgr
            .exec(exec_req)
            .await
            .map_err(|e| RpcError::internal(format!("ward_run exec: {e}")))?;

        let mut rx = mgr
            .stream_output(&args.sandbox_id, &proc_info.pid)
            .await
            .map_err(|e| RpcError::internal(format!("ward_run stream_output: {e}")))?;

        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        let mut exit_code: Option<i32> = None;
        let mut duration_ms: u64 = 0;

        let collect = async {
            while let Some(ev) = rx.recv().await {
                match ev.kind {
                    StreamEventKind::Stdout => stdout_buf.push_str(&ev.line),
                    StreamEventKind::Stderr => stderr_buf.push_str(&ev.line),
                    StreamEventKind::Exit => {
                        exit_code = ev.exit_code;
                        duration_ms = ev.duration_ms;
                    }
                }
            }
        };

        tokio::time::timeout(timeout, collect).await.map_err(|_| {
            RpcError::internal(format!(
                "ward_run: output collection timed out after {}s",
                timeout.as_secs()
            ))
        })?;

        let result = serde_json::json!({
            "pid": proc_info.pid,
            "stdout": stdout_buf,
            "stderr": stderr_buf,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
        });
        Ok(result.to_string())
    }

    /// Forward bytes to a running process's stdin.
    async fn tool_write_stdin(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            pid: String,
            /// UTF-8 data to write to stdin.
            data: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_write_stdin: {e}")))?;

        self.runtime
            .sandbox_manager()
            .write_stdin(
                &args.sandbox_id,
                &args.pid,
                bytes::Bytes::from(args.data.into_bytes()),
            )
            .await
            .map_err(|e| RpcError::internal(format!("write_stdin: {e}")))?;

        Ok(format!(
            "data written to stdin of pid {} in sandbox {}",
            args.pid, args.sandbox_id
        ))
    }

    /// Terminate a running process inside a sandbox.
    async fn tool_kill_process(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            pid: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_kill_process: {e}")))?;

        self.runtime
            .sandbox_manager()
            .kill_process(&args.sandbox_id, &args.pid)
            .await
            .map_err(|e| RpcError::internal(format!("kill_process: {e}")))?;

        Ok(format!(
            "process {} in sandbox {} killed",
            args.pid, args.sandbox_id
        ))
    }

    /// Take a snapshot of a running sandbox.
    async fn tool_create_snapshot(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            label: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_create_snapshot: {e}")))?;

        let info = self
            .runtime
            .sandbox_manager()
            .create_snapshot(&args.sandbox_id, &args.label)
            .await
            .map_err(|e| RpcError::internal(format!("create_snapshot: {e}")))?;

        Ok(format!(
            "snapshot {} created for sandbox {} (label: {})",
            info.snapshot_id, info.sandbox_id, info.label
        ))
    }

    /// Restore a sandbox from a previously-taken snapshot.
    async fn tool_restore_snapshot(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
            snapshot_id: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_restore_snapshot: {e}")))?;

        self.runtime
            .sandbox_manager()
            .restore_snapshot(&args.sandbox_id, &args.snapshot_id)
            .await
            .map_err(|e| RpcError::internal(format!("restore_snapshot: {e}")))?;

        Ok(format!(
            "sandbox {} restored from snapshot {}",
            args.sandbox_id, args.snapshot_id
        ))
    }

    /// List all snapshots taken from a sandbox.
    async fn tool_list_snapshots(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            sandbox_id: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_list_snapshots: {e}")))?;

        let snapshots = self
            .runtime
            .sandbox_manager()
            .list_snapshots(&args.sandbox_id)
            .await
            .map_err(|e| RpcError::internal(format!("list_snapshots: {e}")))?;

        if snapshots.is_empty() {
            return Ok(format!("(no snapshots for sandbox {})", args.sandbox_id));
        }

        let lines: Vec<String> = snapshots
            .iter()
            .map(|s| format!("{}\t{}\t{}", s.snapshot_id, s.label, s.size_bytes))
            .collect();
        Ok(lines.join("\n"))
    }

    /// Allocate a new persistent volume.
    async fn tool_create_volume(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            name: String,
            size_mb: u32,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_create_volume: {e}")))?;

        let req = pb::CreateVolumeRequest {
            name: args.name.clone(),
            size_mb: args.size_mb,
        };
        let info = self
            .runtime
            .volume_manager()
            .create(req)
            .await
            .map_err(|e| RpcError::internal(format!("create_volume: {e}")))?;

        Ok(format!(
            "volume {} created (name: {}, size: {} MB)",
            info.id, info.name, info.size_mb
        ))
    }

    /// List all volumes.
    async fn tool_list_volumes(&self) -> Result<String, RpcError> {
        let volumes = self
            .runtime
            .volume_manager()
            .list()
            .await
            .map_err(|e| RpcError::internal(format!("list_volumes: {e}")))?;

        if volumes.is_empty() {
            return Ok("(no volumes)".into());
        }

        let lines: Vec<String> = volumes
            .iter()
            .map(|v| format!("{}\t{}\t{} MB", v.id, v.name, v.size_mb))
            .collect();
        Ok(lines.join("\n"))
    }

    /// Delete a volume and release its backing storage.
    async fn tool_remove_volume(&self, args: serde_json::Value) -> Result<String, RpcError> {
        #[derive(serde::Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_value(args)
            .map_err(|e| RpcError::invalid_params(format!("ward_remove_volume: {e}")))?;

        self.runtime
            .volume_manager()
            .remove(&args.id)
            .await
            .map_err(|e| RpcError::internal(format!("remove_volume: {e}")))?;

        Ok(format!("volume {} removed", args.id))
    }
}

/// Static schema for all tools we expose. Sent verbatim in `tools/list`.
/// JSON Schema draft-07 conventions; MCP doesn't require a specific dialect
/// but clients (Claude / Cursor) understand draft-07.
fn tools_descriptors() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "ward_create_sandbox",
            "description": "Create a new ward microVM sandbox from an OCI image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": { "type": "string", "description": "OCI image reference, e.g. alpine:latest" },
                    "cpus": { "type": "integer", "minimum": 1, "default": 1 },
                    "memory_mb": { "type": "integer", "minimum": 64, "default": 512 },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Environment variables for the sandbox"
                    }
                },
                "required": ["image"]
            }
        },
        {
            "name": "ward_list_sandboxes",
            "description": "List all current ward sandboxes.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "ward_exec",
            "description": "Execute a command inside a running ward sandbox (fire-and-forget; returns the pid).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Argv array; first element is the executable."
                    },
                    "working_dir": { "type": "string", "description": "Optional working directory" }
                },
                "required": ["sandbox_id", "command"]
            }
        },
        {
            "name": "ward_remove_sandbox",
            "description": "Tear down a ward sandbox and release its resources.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        },
        {
            "name": "ward_run",
            "description": "Execute a command inside a sandbox and collect stdout, stderr, exit_code, and duration_ms synchronously. Waits up to timeout_secs (default 30) for the process to finish.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string", "description": "ID of the target sandbox" },
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Argv array; first element is the executable."
                    },
                    "working_dir": { "type": "string", "description": "Optional working directory inside the sandbox" },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 30,
                        "description": "Max seconds to wait for output collection before returning a timeout error"
                    }
                },
                "required": ["sandbox_id", "command"]
            }
        },
        {
            "name": "ward_write_stdin",
            "description": "Send UTF-8 data to the stdin of a running process inside a sandbox.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" },
                    "pid": { "type": "string", "description": "Process ID returned by ward_exec" },
                    "data": { "type": "string", "description": "UTF-8 bytes to write to stdin" }
                },
                "required": ["sandbox_id", "pid", "data"]
            }
        },
        {
            "name": "ward_kill_process",
            "description": "Terminate a running process inside a ward sandbox.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" },
                    "pid": { "type": "string", "description": "Process ID returned by ward_exec" }
                },
                "required": ["sandbox_id", "pid"]
            }
        },
        {
            "name": "ward_create_snapshot",
            "description": "Take a snapshot of a running sandbox, capturing its current state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" },
                    "label": { "type": "string", "description": "Human-readable label for the snapshot" }
                },
                "required": ["sandbox_id", "label"]
            }
        },
        {
            "name": "ward_restore_snapshot",
            "description": "Restore a sandbox to a previously-taken snapshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" },
                    "snapshot_id": { "type": "string", "description": "Snapshot ID returned by ward_create_snapshot" }
                },
                "required": ["sandbox_id", "snapshot_id"]
            }
        },
        {
            "name": "ward_list_snapshots",
            "description": "List all snapshots taken from a sandbox.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sandbox_id": { "type": "string" }
                },
                "required": ["sandbox_id"]
            }
        },
        {
            "name": "ward_create_volume",
            "description": "Allocate a new persistent ext4 volume that can be attached to sandboxes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Human-readable name for the volume" },
                    "size_mb": { "type": "integer", "minimum": 1, "description": "Volume size in megabytes" }
                },
                "required": ["name", "size_mb"]
            }
        },
        {
            "name": "ward_list_volumes",
            "description": "List all persistent volumes.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "ward_remove_volume",
            "description": "Delete a persistent volume and release its backing storage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Volume ID returned by ward_create_volume" }
                },
                "required": ["id"]
            }
        }
    ])
}
