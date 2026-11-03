// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest setup flags that only a real guest can show. Needs `--features
//! krunvm` and `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_guest_options

#![cfg(feature = "krunvm")]

mod common;

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
/// `df -k` blocks of Docker's default 64 MiB `/dev/shm`.
const DEFAULT_SHM_KIB: &str = "65536";
const CUSTOM_SHM_KIB: &str = "131072";

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

fn run_ok(d: &Daemon, args: &[&str]) -> String {
    let out = d.cli().args(args).output().expect("run ward");
    assert!(
        out.status.success(),
        "{args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// Run `script` in a throwaway sandbox started with `flags` and return what
/// it prints, standard error included.
fn run_script(d: &Daemon, flags: &[&str], script: &str) -> String {
    let image = image();
    let mut args = vec!["run", "--rm"];
    args.extend(flags);
    args.extend([image.as_str(), "sh", "-c", script]);
    run_ok(d, &args)
}

#[test]
fn given_read_only_when_a_file_is_written_then_the_root_fails_and_tmp_run_and_dev_work() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let shared = tempfile::tempdir_in("/tmp").expect("tempdir");
    let bind = format!("{}:/shared", shared.path().display());
    let script = "touch /rootfile 2>&1; \
                  touch /tmp/t && echo tmp-ok; \
                  touch /run/r && echo run-ok; \
                  touch /dev/shm/s && echo shm-ok; \
                  touch /scratch/s && echo tmpfs-ok; \
                  touch /shared/s && echo volume-ok";

    // Act
    let out = run_script(
        &d,
        &["--read-only", "--tmpfs", "/scratch", "-v", &bind],
        script,
    );

    // Assert
    assert!(out.contains("Read-only file system"), "{out}");
    for ok in ["tmp-ok", "run-ok", "shm-ok", "tmpfs-ok", "volume-ok"] {
        assert!(out.contains(ok), "{ok} missing from {out}");
    }
    assert!(shared.path().join("s").exists());
}

#[test]
fn given_a_tmpfs_size_when_it_is_filled_then_the_write_stops_at_the_limit() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();

    // Act
    let out = run_script(
        &d,
        &["--tmpfs", "/scratch:size=1m"],
        "dd if=/dev/zero of=/scratch/f bs=1k count=2048 2>&1; \
         echo size=$(stat -c %s /scratch/f)",
    );

    // Assert
    assert!(out.contains("No space left on device"), "{out}");
    assert!(out.contains("size=1048576"), "{out}");
}

#[test]
fn given_noexec_default_and_exec_option_when_a_binary_runs_then_docker_defaults_apply() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let script = "cp /bin/true /a/true; cp /bin/true /b/true; \
                  /a/true 2>&1 && echo a-ran; /b/true 2>&1 && echo b-ran";

    // Act
    let out = run_script(&d, &["--tmpfs", "/a", "--tmpfs", "/b:exec"], script);

    // Assert
    assert!(!out.contains("a-ran"), "{out}");
    assert!(out.contains("b-ran"), "{out}");
}

#[test]
fn given_shm_size_when_df_runs_then_dev_shm_has_that_size_and_the_default_is_64m() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let script = "df -k /dev/shm | tail -1 | tr -s ' ' | cut -d' ' -f2";

    // Act
    let custom = run_script(&d, &["--shm-size", "128m"], script);
    let default = run_script(&d, &[], script);

    // Assert
    assert_eq!(custom.trim(), CUSTOM_SHM_KIB);
    assert_eq!(default.trim(), DEFAULT_SHM_KIB);
}

#[test]
fn given_mount_flags_when_run_then_bind_is_read_only_and_tmpfs_is_sized() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let shared = tempfile::tempdir_in("/tmp").expect("tempdir");
    let bind = format!(
        "type=bind,src={},dst=/shared,readonly",
        shared.path().display()
    );
    let script = "touch /shared/s 2>&1; \
                  df -k /cache | tail -1 | tr -s ' ' | cut -d' ' -f2";

    // Act
    let out = run_script(
        &d,
        &[
            "--mount",
            &bind,
            "--mount",
            "type=tmpfs,dst=/cache,tmpfs-size=2m",
        ],
        script,
    );

    // Assert
    assert!(out.contains("Read-only file system"), "{out}");
    assert!(out.trim().ends_with("2048"), "{out}");
}
