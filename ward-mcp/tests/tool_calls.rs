// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! tools/call integration tests for ward-mcp.
//!
//! `tests/handshake.rs` covers initialize / tools/list / unknown-method.
//! This file drives the real tools -- each test sends a `tools/call`
//! frame and asserts on the response shape, including round-tripping
//! state across calls (create -> list -> remove).
//!
//! Each test spawns its own `ward-mcp` subprocess against a per-test
//! WARD_DATA_DIR so they can run in parallel without colliding.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

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
            // WARD_OCI_OFFLINE=1: skip real registry pulls so MCP tool
            // tests stay hermetic on egress-blocked CI runners.
            .env("WARD_OCI_OFFLINE", "1")
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
        let arr = resp["result"]["content"].as_array().expect("content array");
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

// ── Original 4 tools ─────────────────────────────────────────────────────────

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
    // results when the list is empty -- silently returning "" would be
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

// ── New tools ─────────────────────────────────────────────────────────────────

#[test]
fn given_sandbox_when_ward_run_then_returns_json_result() {
    // ward_run calls exec then stream_output and returns a JSON object
    // with stdout/stderr/exit_code/duration_ms. The stub backend produces
    // a process that exits immediately; we just verify the envelope shape.
    let mut srv = McpServer::spawn();

    // Create a sandbox first.
    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let create_text = McpServer::tool_text(&cr);
    let sandbox_id = create_text
        .split_whitespace()
        .nth(1)
        .expect("sandbox id in create text")
        .to_string();

    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_run",
            "arguments": {
                "sandbox_id": sandbox_id,
                "command": ["echo", "hello"]
            }
        }),
    );
    let text = McpServer::tool_text(&resp);
    // The text is a JSON object; verify it parses and has the expected keys.
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("ward_run must return JSON text");
    assert!(parsed["pid"].is_string(), "result must have pid: {parsed}");
    assert!(
        parsed["stdout"].is_string(),
        "result must have stdout: {parsed}"
    );
    assert!(
        parsed["stderr"].is_string(),
        "result must have stderr: {parsed}"
    );
    assert!(
        !parsed["duration_ms"].is_null(),
        "result must have duration_ms: {parsed}"
    );
}

#[test]
fn given_sandbox_exec_when_ward_write_stdin_then_succeeds() {
    let mut srv = McpServer::spawn();

    // Create sandbox.
    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    // Start a process to get a pid.
    let er = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_exec",
            "arguments": { "sandbox_id": sandbox_id.clone(), "command": ["cat"] }
        }),
    );
    let exec_text = McpServer::tool_text(&er);
    // "started pid <pid> in sandbox <id>"
    let pid = exec_text
        .split_whitespace()
        .nth(2)
        .expect("pid token in exec text")
        .to_string();

    let resp = srv.round_trip(
        3,
        "tools/call",
        json!({
            "name": "ward_write_stdin",
            "arguments": {
                "sandbox_id": sandbox_id.clone(),
                "pid": pid.clone(),
                "data": "hello\n"
            }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(text.contains(&pid), "response must mention the pid: {text}");
}

#[test]
fn given_sandbox_exec_when_ward_kill_process_then_succeeds() {
    let mut srv = McpServer::spawn();

    // Create sandbox.
    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    // Start a process.
    let er = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_exec",
            "arguments": { "sandbox_id": sandbox_id.clone(), "command": ["sleep", "100"] }
        }),
    );
    let exec_text = McpServer::tool_text(&er);
    let pid = exec_text
        .split_whitespace()
        .nth(2)
        .expect("pid token in exec text")
        .to_string();

    let resp = srv.round_trip(
        3,
        "tools/call",
        json!({
            "name": "ward_kill_process",
            "arguments": { "sandbox_id": sandbox_id.clone(), "pid": pid.clone() }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains(&pid),
        "kill confirmation must mention the pid: {text}"
    );
}

#[test]
fn given_sandbox_when_ward_create_snapshot_then_returns_snapshot_id() {
    let mut srv = McpServer::spawn();

    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_create_snapshot",
            "arguments": { "sandbox_id": sandbox_id.clone(), "label": "v1" }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("snapshot"),
        "response must mention snapshot: {text}"
    );
    assert!(
        text.contains(&sandbox_id),
        "response must mention sandbox id: {text}"
    );
}

#[test]
fn given_snapshot_when_ward_restore_snapshot_then_succeeds() {
    let mut srv = McpServer::spawn();

    // Create sandbox.
    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    // Take a snapshot.
    let sr = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_create_snapshot",
            "arguments": { "sandbox_id": sandbox_id.clone(), "label": "checkpoint" }
        }),
    );
    let snap_text = McpServer::tool_text(&sr);
    // "snapshot <snapshot_id> created for sandbox <sandbox_id> (label: checkpoint)"
    let snapshot_id = snap_text
        .split_whitespace()
        .nth(1)
        .expect("snapshot_id in snapshot text")
        .to_string();

    let resp = srv.round_trip(
        3,
        "tools/call",
        json!({
            "name": "ward_restore_snapshot",
            "arguments": {
                "sandbox_id": sandbox_id.clone(),
                "snapshot_id": snapshot_id.clone()
            }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains(&sandbox_id),
        "restore confirmation must mention sandbox: {text}"
    );
    assert!(
        text.contains(&snapshot_id),
        "restore confirmation must mention snapshot: {text}"
    );
}

#[test]
fn given_sandbox_with_snapshot_when_ward_list_snapshots_then_appears() {
    let mut srv = McpServer::spawn();

    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    // Take a snapshot so there is something to list.
    srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_create_snapshot",
            "arguments": { "sandbox_id": sandbox_id.clone(), "label": "snap1" }
        }),
    );

    let resp = srv.round_trip(
        3,
        "tools/call",
        json!({
            "name": "ward_list_snapshots",
            "arguments": { "sandbox_id": sandbox_id.clone() }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("snap1"),
        "list_snapshots must include the label: {text}"
    );
}

#[test]
fn given_sandbox_no_snapshots_when_ward_list_snapshots_then_explicit_empty_marker() {
    let mut srv = McpServer::spawn();

    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_create_sandbox", "arguments": { "image": "alpine" } }),
    );
    let sandbox_id = McpServer::tool_text(&cr)
        .split_whitespace()
        .nth(1)
        .expect("sandbox id")
        .to_string();

    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_list_snapshots",
            "arguments": { "sandbox_id": sandbox_id.clone() }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("no snapshots"),
        "empty snapshot list must emit explicit marker: {text}"
    );
}

#[test]
fn given_ward_create_volume_when_called_then_returns_volume_id() {
    let mut srv = McpServer::spawn();

    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_volume",
            "arguments": { "name": "my-vol", "size_mb": 64 }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("volume") && text.contains("my-vol"),
        "create_volume response must mention volume and name: {text}"
    );
}

#[test]
fn given_no_volumes_when_ward_list_volumes_then_explicit_empty_marker() {
    let mut srv = McpServer::spawn();

    let resp = srv.round_trip(
        1,
        "tools/call",
        json!({ "name": "ward_list_volumes", "arguments": {} }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("(no volumes)"),
        "empty list must emit explicit marker: {text}"
    );
}

#[test]
fn given_created_volume_when_ward_list_volumes_then_appears() {
    let mut srv = McpServer::spawn();

    srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_volume",
            "arguments": { "name": "data-vol", "size_mb": 128 }
        }),
    );

    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({ "name": "ward_list_volumes", "arguments": {} }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains("data-vol"),
        "list_volumes must include the created volume: {text}"
    );
}

#[test]
fn given_created_volume_when_ward_remove_volume_then_gone() {
    let mut srv = McpServer::spawn();

    let cr = srv.round_trip(
        1,
        "tools/call",
        json!({
            "name": "ward_create_volume",
            "arguments": { "name": "tmp-vol", "size_mb": 32 }
        }),
    );
    let create_text = McpServer::tool_text(&cr);
    // "volume <id> created (name: tmp-vol, size: 32 MB)"
    let vol_id = create_text
        .split_whitespace()
        .nth(1)
        .expect("volume id in create text")
        .to_string();

    let resp = srv.round_trip(
        2,
        "tools/call",
        json!({
            "name": "ward_remove_volume",
            "arguments": { "id": vol_id.clone() }
        }),
    );
    let text = McpServer::tool_text(&resp);
    assert!(
        text.contains(&vol_id),
        "remove_volume must confirm the id: {text}"
    );

    // Verify it no longer appears in list.
    let lr = srv.round_trip(
        3,
        "tools/call",
        json!({ "name": "ward_list_volumes", "arguments": {} }),
    );
    let list_text = McpServer::tool_text(&lr);
    assert!(
        !list_text.contains(&vol_id),
        "removed volume must not appear in list: {list_text}"
    );
}
