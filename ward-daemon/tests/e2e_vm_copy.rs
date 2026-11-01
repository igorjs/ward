// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward cp` against a real guest agent.
//!
//! The fake guest of the daemon tests cannot show how the real agent
//! unpacks, packs and reports errors, so these run the whole path: CLI,
//! daemon, vsock, agent, and back. Needs `--features krunvm` and
//! `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_copy

#![cfg(feature = "krunvm")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
/// Big enough to span several tar chunks on the way through the agent.
const BIG_FILE_BYTES: usize = 3 * 1024 * 1024;
/// Multiplier of a full-period generator mod 256, so the bytes cover every value.
const BYTE_STEP: usize = 197;
const EXECUTABLE_MODE: u32 = 0o755;
const PRIVATE_MODE: u32 = 0o600;
const MODE_MASK: u32 = 0o777;
const MISSING_PREFIX: &str = "Error response from daemon: Could not find the file";

macro_rules! require_vm {
    () => {
        if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
            eprintln!("skipped: set {ENABLE_ENV}=1 to run real-VM tests");
            return;
        }
    };
}

fn image() -> String {
    std::env::var(IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn ok_stdout(out: &Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
}

/// A running daemon with one booted sandbox.
struct Guest {
    daemon: Daemon,
    id: String,
    host: tempfile::TempDir,
}

impl Guest {
    fn boot() -> Self {
        let daemon = Daemon::spawn_vm();
        let out = daemon
            .cli()
            .args(["create", &image()])
            .output()
            .expect("run ward create");
        let id = ok_stdout(&out, "create").trim().to_string();
        Self {
            daemon,
            id,
            host: tempfile::tempdir().expect("host scratch dir"),
        }
    }

    fn host(&self, name: &str) -> String {
        self.host.path().join(name).to_string_lossy().into_owned()
    }

    fn sandbox(&self, path: &str) -> String {
        format!("{}:{path}", self.id)
    }

    fn cp(&self, from: &str, to: &str) -> Output {
        let mut cmd: Command = self.daemon.cli();
        cmd.args(["cp", from, to]);
        cmd.output().expect("run ward cp")
    }

    fn cp_ok(&self, from: &str, to: &str) {
        ok_stdout(&self.cp(from, to), "cp");
    }

    /// Run `script` in the guest and return its output.
    fn sh(&self, script: &str) -> String {
        let out = self
            .daemon
            .cli()
            .args(["exec", &self.id, "--", "sh", "-c", script])
            .output()
            .expect("run ward exec");
        ok_stdout(&out, "exec")
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * BYTE_STEP) as u8).collect()
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & MODE_MASK
}

#[test]
fn given_a_binary_file_when_copied_in_and_out_then_the_bytes_and_mode_are_identical() {
    require_vm!();
    let guest = Guest::boot();
    let source = guest.host("blob");
    fs::write(&source, pattern(BIG_FILE_BYTES)).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(PRIVATE_MODE)).unwrap();

    guest.cp_ok(&source, &guest.sandbox("/tmp/blob"));
    let back = guest.host("back");
    guest.cp_ok(&guest.sandbox("/tmp/blob"), &back);

    assert_eq!(fs::read(&back).unwrap(), pattern(BIG_FILE_BYTES));
    assert_eq!(mode_of(Path::new(&back)), PRIVATE_MODE);
}

#[test]
fn given_a_directory_tree_when_copied_in_and_out_then_the_tree_is_identical() {
    require_vm!();
    let guest = Guest::boot();
    let tree = guest.host("tree");
    fs::create_dir_all(format!("{tree}/deep/er")).unwrap();
    fs::create_dir(format!("{tree}/empty")).unwrap();
    fs::write(format!("{tree}/deep/er/data.bin"), pattern(BIG_FILE_BYTES)).unwrap();
    fs::write(format!("{tree}/run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(
        format!("{tree}/run.sh"),
        fs::Permissions::from_mode(EXECUTABLE_MODE),
    )
    .unwrap();
    std::os::unix::fs::symlink("run.sh", format!("{tree}/link")).unwrap();

    guest.cp_ok(&tree, &guest.sandbox("/tmp"));
    let back = guest.host("back");
    fs::create_dir(&back).unwrap();
    guest.cp_ok(&guest.sandbox("/tmp/tree"), &back);

    let copy = format!("{back}/tree");
    assert_eq!(
        fs::read(format!("{copy}/deep/er/data.bin")).unwrap(),
        pattern(BIG_FILE_BYTES)
    );
    assert!(Path::new(&format!("{copy}/empty")).is_dir());
    assert_eq!(
        mode_of(Path::new(&format!("{copy}/run.sh"))),
        EXECUTABLE_MODE
    );
    assert_eq!(
        fs::read_link(format!("{copy}/link")).unwrap(),
        Path::new("run.sh")
    );
}

#[test]
fn given_a_missing_guest_path_when_copied_out_then_the_error_is_docker_not_found() {
    require_vm!();
    let guest = Guest::boot();

    let out = guest.cp(&guest.sandbox("/nope"), &guest.host("out"));

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with(MISSING_PREFIX), "{stderr}");
    assert!(stderr.contains("/nope in container"), "{stderr}");
    assert!(!Path::new(&guest.host("out")).exists());
}

#[test]
fn given_a_missing_guest_parent_when_copied_in_then_the_error_is_docker_not_found() {
    require_vm!();
    let guest = Guest::boot();
    let source = guest.host("f");
    fs::write(&source, b"x").unwrap();

    let out = guest.cp(&source, &guest.sandbox("/nope/dir/f"));

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with(MISSING_PREFIX), "{stderr}");
}

#[test]
fn given_dir_contents_when_copied_in_then_they_land_without_the_source_dir_name() {
    require_vm!();
    let guest = Guest::boot();
    let src = guest.host("src");
    fs::create_dir(&src).unwrap();
    fs::write(format!("{src}/a.txt"), b"a").unwrap();
    guest.sh("mkdir /tmp/existing");

    guest.cp_ok(&format!("{src}/."), &guest.sandbox("/tmp/existing"));
    guest.cp_ok(&format!("{src}/."), &guest.sandbox("/tmp/created"));

    let listing = guest.sh("ls /tmp/existing /tmp/created");
    assert_eq!(listing.matches("a.txt").count(), 2, "{listing}");
    assert!(!listing.contains("src"), "{listing}");
}

#[test]
fn given_dir_contents_when_copied_in_under_a_missing_parent_then_it_fails_and_creates_nothing() {
    require_vm!();
    let guest = Guest::boot();
    let src = guest.host("src");
    fs::create_dir(&src).unwrap();
    fs::write(format!("{src}/a.txt"), b"a").unwrap();

    let out = guest.cp(&format!("{src}/."), &guest.sandbox("/nope/created"));

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with(MISSING_PREFIX));
    assert!(guest.sh("ls -d /nope 2>&1 || true").contains("No such"));
}
