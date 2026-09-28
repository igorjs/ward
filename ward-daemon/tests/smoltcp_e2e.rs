// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Real-hardware smoke test for the smoltcp network backend.
//!
//! Everything else that exercises the smoltcp `Stack` runs against fake
//! hardware (a `socketpair` standing in for the virtio-net device) so it
//! can run in ordinary CI. This file is the one test that boots an actual
//! sandbox through real libkrun on real KVM/HVF and proves the assembled
//! backend does what a user needs: resolve a hostname and complete a TCP
//! connection from inside the guest. It needs a machine this workspace's
//! CI runners do not have, so it stays `#[ignore]`d; run it manually with
//! `cargo test -p ward-daemon --features krunvm --test smoltcp_e2e --
//! --ignored` on a host with libkrun installed and `/dev/kvm` (or HVF on
//! macOS) available.

#![cfg(feature = "krunvm")]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use predicates::prelude::*;

/// Pulls the value out of a `<prefix>value` line in `ward` CLI stdout,
/// mirroring the sibling e2e files' `extract_field` helper for chaining
/// `create` -> `exec` -> `logs` by id/pid.
fn extract_field(stdout: &str, prefix: &str) -> String {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    panic!("no {prefix:?} line in stdout:\n{stdout}");
}

#[test]
#[ignore]
fn given_default_backend_when_sandbox_created_then_dns_and_tcp_egress_work() {
    // Arrange: a bespoke `wardd` spawn (rather than `common::Daemon::spawn`)
    // because this scenario needs WARD_NETWORK_BACKEND, which the shared
    // harness does not parameterise, and a real (non-offline) image pull
    // so the guest has `getent`/`wget` available.
    let data_dir = tempfile::tempdir().expect("tempdir");
    let socket = data_dir.path().join("ward.sock");

    let mut wardd = Command::cargo_bin("wardd")
        .unwrap()
        .env("WARD_SOCKET", &socket)
        .env("WARD_DATA_DIR", data_dir.path())
        .env("WARD_LOG_LEVEL", "warn")
        .env("WARD_NETWORK_BACKEND", "smoltcp")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wardd");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if !socket.exists() {
        let _ = wardd.kill();
        let _ = wardd.wait();
        panic!("wardd did not bind socket within 5s: {}", socket.display());
    }

    let ward = || {
        let mut cmd = assert_cmd::Command::cargo_bin("ward").unwrap();
        cmd.env("WARD_SOCKET", &socket);
        cmd
    };

    // Act: boot a real sandbox on the smoltcp backend.
    let create_out = ward()
        .args(["create", "alpine:latest"])
        .output()
        .expect("create");
    assert!(
        create_out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&create_out.stderr)
    );
    let id = extract_field(std::str::from_utf8(&create_out.stdout).unwrap(), "id: ");

    // Act: resolve a real hostname from inside the guest via the
    // backend's DNS relay.
    let dns_exec = ward()
        .args(["exec", &id, "--", "getent", "hosts", "example.com"])
        .output()
        .expect("exec getent");
    assert!(
        dns_exec.status.success(),
        "exec getent failed: {}",
        String::from_utf8_lossy(&dns_exec.stderr)
    );
    let dns_pid = extract_field(std::str::from_utf8(&dns_exec.stdout).unwrap(), "pid: ");

    // Assert: the resolver returned an address and the guest process
    // exited cleanly.
    ward()
        .args(["logs", &id, &dns_pid])
        .assert()
        .success()
        .stdout(predicate::str::contains("stdout:"))
        .stdout(predicate::str::contains("exit: 0"));

    // Act: complete one real TCP connection through the backend's egress
    // path.
    let tcp_exec = ward()
        .args([
            "exec",
            &id,
            "--",
            "wget",
            "-q",
            "-O",
            "/dev/null",
            "https://example.com",
        ])
        .output()
        .expect("exec wget");
    assert!(
        tcp_exec.status.success(),
        "exec wget failed: {}",
        String::from_utf8_lossy(&tcp_exec.stderr)
    );
    let tcp_pid = extract_field(std::str::from_utf8(&tcp_exec.stdout).unwrap(), "pid: ");

    // Assert: the guest's TCP connect/handshake/transfer completed and
    // the process exited cleanly.
    ward()
        .args(["logs", &id, &tcp_pid])
        .assert()
        .success()
        .stdout(predicate::str::contains("exit: 0"));

    // Teardown: reap the daemon explicitly so a failed assertion above
    // does not leak a background wardd process.
    let _ = wardd.kill();
    let _ = wardd.wait();
}
