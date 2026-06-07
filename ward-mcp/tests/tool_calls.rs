// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! tools/call integration tests for ward-mcp.
//!
//! `tests/handshake.rs` covers initialize / tools/list / unknown-method.
//! This file drives the real tools — each test sends a `tools/call`
//! frame and asserts on the response shape, including round-tripping
//! state across calls (create → list → remove).
//!
//! Each test spawns its own `ward-mcp` subprocess against a per-test
//! WARD_DATA_DIR so they can run in parallel without colliding.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// RAII guard around a running ward-mcp subprocess wired to a temp
/// data dir. SIGKILLs on drop. Per-test isolation.
struct McpServer {
    _tmp: tempfile::TempDir,
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpServer {
    fn spawn() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = env!("CARGO_BIN_EXE_ward-mcp");
        let mut child = Command::new(bin)
            .env("WARD_DATA_DIR", tmp.path())
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ward-mcp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        Self {
            _tmp: tmp,
            child: Some(child),
            stdin,
            stdout,
        }
    }

    /// Send one JSON-RPC request and read one response. Panics if the
    /// daemon emits anything that isn't valid JSON or doesn't carry an
    /// `id` matching the request.
    fn round_trip(&mut self, id: i64, method: &str, params: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{req}").expect("write request");

        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let resp: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON response {line:?}: {e}"));
        assert_eq!(resp["id"], id, "id mismatch in response: {resp}");
        resp
    }

    /// Parse the canonical MCP `tools/call` result envelope. Asserts
    /// shape, returns the body text of the first content item.
    fn tool_text(resp: &Value) -> String {
        assert!(resp["error"].is_null(), "tools/call returned error: {resp}");
        assert_eq!(
            resp["result"]["isError"], false,
            "isError flag must be false on success: {resp}"
        );
        let arr = resp["result"]["content"]
            .as_array()
            .expect("content array");
        assert!(!arr.is_empty(), "content must be non-empty: {resp}");
        arr[0]["text"]
            .as_str()
            .expect("content[0].text must be a string")
            .to_string()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn given_create_sandbox_when_called_then_returns_sandbox_id() {
    let mut srv = McpServer::spawn();
    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_sandbox",
            "arguments": { "image": "alpine", "cpus": 1, "memory_mb": 256 }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("sandbox") && text.contains("alpine"),
        "expected create-confirmation text, got: {text}"
    );
}

#[test]
fn given_created_sandbox_when_list_then_appears() {
    // Regression: state persists across tool calls within one process.
    let mut srv = McpServer::spawn();
    srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_sandbox",
            "arguments": { "image": "alpine" }
        }),
    );
    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({ "name": "ward_list_sandboxes", "arguments": {} }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("alpine"),
        "list should show the created sandbox: {text}"
    );
}

#[test]
fn given_no_sandboxes_when_list_then_explicit_empty_marker() {
    // Explicit "(no sandboxes)" string keeps the LLM from hallucinating
    // results when the list is empty — silently returning "" would be
    // ambiguous to the agent.
    let mut srv = McpServer::spawn();
    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_list_sandboxes", "arguments": {} }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("(no sandboxes)"),
        "empty list must emit explicit marker, got: {text}"
    );
}

#[test]
fn given_unknown_tool_when_called_then_invalid_params_error() {
    let mut srv = McpServer::spawn();
    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_does_not_exist", "arguments": {} }),
    );
    // Unknown tool surfaces as JSON-RPC -32602 (invalid params) per
    // ward-mcp's tools/call dispatch contract.
    assert_eq!(resp["error"]["code"], -32602, "expected -32602: {resp}");
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("ward_does_not_exist"),
        "error must name the bad tool: {msg}"
    );
}

#[test]
fn given_create_then_remove_when_list_then_gone() {
    // Full lifecycle through MCP: confirms that ward_remove_sandbox
    // actually plumbs through to the runtime's SandboxManager::remove.
    let mut srv = McpServer::spawn();

    let create_resp = srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_sandbox",
            "arguments": { "image": "alpine" }
        }),
    );
    let create_text = McpServer::tool_text(&create_resp);
    // Extract the id from "sandbox <id> created from alpine".
    let id = create_text
        .split_whitespace()
        .nth(1)
        .expect("id token in create text")
        .to_string();

    srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_remove_sandbox",
            "arguments": { "id": id.clone() }
        }),
    );

    let list_resp = srv.round_trip(
        3,
        "tools/call",
        json!({ "name": "ward_list_sandboxes", "arguments": {} }),
    );
    let list_text = McpServer::tool_text(&list_resp);
    assert!(
        !list_text.contains(&id),
        "removed sandbox {id} must not appear in list: {list_text}"
    );
}

#[test]
fn given_invalid_arguments_when_called_then_invalid_params_error() {
    // Missing required `image` field on ward_create_sandbox. Should
    // surface as -32602 with a message naming the tool.
    let mut srv = McpServer::spawn();
    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_sandbox",
            "arguments": {}  // missing image
        }),
    );
    assert_eq!(resp["error"]["code"], -32602, "expected -32602: {resp}");
}
