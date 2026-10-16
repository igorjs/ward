// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! ward-mcp against a real microVM.
//!
//! Speaks the MCP protocol over stdio to a real `ward-mcp` process in embedded
//! mode: create a sandbox, run a command in it, remove it. Needs a hypervisor
//! and a registry. Opt in with `WARD_E2E_VM=1` and `--features krunvm`;
//! `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-mcp --features krunvm --test e2e_vm

#![cfg(feature = "krunvm")]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const DEFAULT_IMAGE: &str = "alpine:3.20";

struct McpServer {
    _data: tempfile::TempDir,
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpServer {
    fn spawn() -> Self {
        let data = tempfile::tempdir().expect("tempdir");
        let bin = env!("CARGO_BIN_EXE_ward-mcp");
        sign_for_hypervisor(bin);
        let runtime = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ward-mcp-e2e-runtime");
        let mut child = Command::new(bin)
            .env("WARD_DATA_DIR", data.path())
            .env("WARD_RUNTIME_DIR", runtime)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn ward-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            _data: data,
            child: Some(child),
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}").expect("write request");
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let resp: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON response {line:?}: {e}"));
        assert_eq!(resp["id"], id, "id mismatch: {resp}");
        resp
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let resp = self.call(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        assert!(resp["error"].is_null(), "{name} returned an error: {resp}");
        assert_eq!(resp["result"]["isError"], false, "{name} failed: {resp}");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: no text content in {resp}"));
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
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

/// macOS only: the hypervisor entitlement is required to start a VM.
fn sign_for_hypervisor(binary: &str) {
    static SIGNED: std::sync::Once = std::sync::Once::new();
    SIGNED.call_once(|| {
        if !cfg!(target_os = "macos") {
            return;
        }
        let entitlements = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("entitlements.plist");
        let status = Command::new("codesign")
            .args(["--force", "--sign", "-", "--entitlements"])
            .arg(entitlements)
            .arg(binary)
            .status()
            .expect("run codesign");
        assert!(status.success(), "codesign failed");
    });
}

#[test]
fn given_mcp_client_when_creating_running_and_removing_a_sandbox_then_commands_execute_in_a_real_vm()
 {
    if std::env::var("WARD_E2E_VM").as_deref() != Ok("1") {
        eprintln!("skipped: set WARD_E2E_VM=1 to run real-VM tests");
        return;
    }
    let image = std::env::var("WARD_E2E_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string());
    let mut srv = McpServer::spawn();

    let created = srv.tool(
        "ward_create_sandbox",
        json!({ "image": image, "cpus": 1, "memory_mb": 256 }),
    );
    // Reply text is "sandbox <id> created from <image>".
    let sandbox_id = created
        .as_str()
        .and_then(|text| text.split_whitespace().nth(1))
        .unwrap_or_else(|| panic!("no sandbox id in {created}"))
        .to_string();
    let ran = srv.tool(
        "ward_run",
        json!({ "sandbox_id": sandbox_id, "command": ["sh", "-c", "uname -s; echo mcp-ok"] }),
    );
    let removed = srv.call(
        "tools/call",
        json!({ "name": "ward_remove_sandbox", "arguments": { "id": sandbox_id } }),
    );

    let stdout = ran["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("Linux"), "guest is not Linux: {ran}");
    assert!(stdout.contains("mcp-ok"), "command output missing: {ran}");
    assert_eq!(ran["exit_code"], 0, "{ran}");
    assert!(removed["error"].is_null(), "remove failed: {removed}");
}
