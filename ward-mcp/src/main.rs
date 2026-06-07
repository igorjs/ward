// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-mcp: Model Context Protocol server exposing ward sandboxes to LLM agents.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout per the MCP transport spec. Boots
//! an in-process `ward-runtime` so the agent doesn't need a separate
//! `wardd` running. Per ADR-016, an MCP server is a per-process owner of
//! its sandboxes, so embedded mode is the natural fit.
//!
//! Supported MCP methods:
//!   - `initialize`            — handshake + capability negotiation
//!   - `tools/list`            — enumerate available tools
//!   - `tools/call`            — invoke one of the tools
//!   - `notifications/initialized` — accepted, no-op
//!
//! Tools exposed:
//!   - `ward_create_sandbox`   — boot a microVM from an OCI image
//!   - `ward_list_sandboxes`   — list current sandboxes
//!   - `ward_exec`             — run a command (synchronous capture)
//!   - `ward_remove_sandbox`   — tear a sandbox down
//!
//! See `docs/adr/016-embedded-mode-microvms.md`.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use ward_core::pb;
use ward_runtime::Runtime;

mod rpc;

use rpc::{Error as RpcError, Request, Response};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "ward-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // MCP servers are spawned by clients (Claude / Cursor / ...) which
    // own the stdio pair. Logs MUST go to stderr — stdout is the wire
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
        // something else (e.g. `ward` CLI) — print a hint instead of
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

            // Notifications (no `id`) produce no response — skip the write.
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
}

/// Static schema for the four tools we expose. Sent verbatim in
/// `tools/list`. JSON Schema draft-07 conventions; MCP doesn't require a
/// specific dialect but clients (Claude / Cursor) understand draft-07.
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
        }
    ])
}
