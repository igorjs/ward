// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end smoke test: spawn the `ward-mcp` binary, write one
//! JSON-RPC frame to stdin, read one frame from stdout, assert on
//! shape. Catches regressions in initialize / tools-list wiring
//! without standing up a full MCP client.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

/// Run the compiled `ward-mcp` binary with a per-test data dir, send it
/// `request`, and return the parsed response object.
fn send_one(request: &str) -> Value {
    // tempfile path for WARD_DATA_DIR — the binary will create the
    // tree on first use.
    let tmp = tempfile::tempdir().expect("tempdir");

    // assert_cmd is a dev-dep of ward-daemon; we don't pull it in for
    // ward-mcp, so resolve the binary path the long way.
    let bin = env!("CARGO_BIN_EXE_ward-mcp");

    let mut child = Command::new(bin)
        .env("WARD_DATA_DIR", tmp.path())
        .env("RUST_LOG", "error") // quiet logs in test output
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ward-mcp");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", request).expect("write request");
    }

    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");

    // Kill the child so stdin EOF doesn't matter — we have what we need.
    let _ = child.kill();
    let _ = child.wait();

    serde_json::from_str(&line).expect("parse response JSON")
}

#[test]
fn given_initialize_request_when_server_responds_then_advertises_tools_capability() {
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let resp = send_one(req);

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object(), "expected result object");
    assert_eq!(resp["result"]["serverInfo"]["name"], "ward-mcp");
    // Capability advertisement is what MCP clients key off to know
    // whether to call tools/list at all.
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "missing tools capability: {resp}"
    );
}

#[test]
fn given_tools_list_request_when_server_responds_then_returns_four_tools() {
    let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let resp = send_one(req);

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    let tools = resp["result"]["tools"].as_array().expect("tools is array");
    assert_eq!(tools.len(), 4, "expected 4 tools: {resp}");

    // Names are the SDK-stable surface; pin the set so adding/removing
    // requires a deliberate test edit.
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"ward_create_sandbox"));
    assert!(names.contains(&"ward_list_sandboxes"));
    assert!(names.contains(&"ward_exec"));
    assert!(names.contains(&"ward_remove_sandbox"));
}

#[test]
fn given_unknown_method_when_server_responds_then_returns_method_not_found() {
    let req = r#"{"jsonrpc":"2.0","id":7,"method":"definitely/not/a/method"}"#;
    let resp = send_one(req);

    assert_eq!(resp["id"], 7);
    assert!(resp["result"].is_null() || resp["result"].is_object().eq(&false));
    // JSON-RPC 2.0 §5.1: -32601 = Method not found.
    assert_eq!(resp["error"]["code"], -32601, "expected -32601 in {resp}");
}
