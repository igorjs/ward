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

#[test]
fn given_add_host_and_resolver_flags_when_run_then_the_guest_files_have_them() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let script = "getent hosts db.internal; cat /etc/resolv.conf";

    // Act
    let out = run_script(
        &d,
        &[
            "--add-host",
            "db.internal:10.1.2.3",
            "--dns-search",
            "corp.example",
            "--dns-option",
            "ndots:3",
        ],
        script,
    );

    // Assert
    assert!(out.contains("10.1.2.3"), "{out}");
    assert!(out.contains("search corp.example"), "{out}");
    assert!(out.contains("options ndots:3"), "{out}");
}

#[test]
fn given_ulimit_and_group_add_when_run_and_exec_then_both_see_them() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let id = run_ok(
        &d,
        &[
            "run",
            "-d",
            "--ulimit",
            "nofile=512:1024",
            "--ulimit",
            "core=0",
            "--group-add",
            "1234",
            "--group-add",
            "audio",
            &image,
            "sleep",
            "60",
        ],
    );
    let id = id.trim();

    // Act
    let main = run_script(
        &d,
        &["--ulimit", "nofile=256:256", "--group-add", "1234"],
        "ulimit -n; ulimit -Hn; id -G",
    );
    let exec_soft = run_ok(&d, &["exec", id, "sh", "-c", "ulimit -n"]);
    let exec_hard = run_ok(&d, &["exec", id, "sh", "-c", "ulimit -Hn"]);
    let exec_core = run_ok(&d, &["exec", id, "sh", "-c", "ulimit -c"]);
    let exec_groups = run_ok(&d, &["exec", id, "id", "-G"]);

    // Assert
    let lines: Vec<&str> = main.lines().collect();
    assert_eq!(&lines[..2], ["256", "256"], "{main}");
    assert!(lines[2].split_whitespace().any(|g| g == "1234"), "{main}");
    assert_eq!(exec_soft.trim(), "512");
    assert_eq!(exec_hard.trim(), "1024");
    assert_eq!(exec_core.trim(), "0");
    let groups: Vec<&str> = exec_groups.split_whitespace().collect();
    assert!(groups.contains(&"1234"), "{exec_groups}");
    assert!(groups.contains(&"18"), "{exec_groups}");
}

#[test]
fn given_an_unknown_group_name_when_run_then_the_process_does_not_start() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();

    // Act
    let out = d
        .cli()
        .args(["run", "--rm", "--group-add", "nosuchgroup", &image, "true"])
        .output()
        .expect("run ward");

    // Assert
    assert!(!out.status.success());
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("unable to find group nosuchgroup"), "{text}");
}

const HEALTH_POLL_STEP: std::time::Duration = std::time::Duration::from_millis(500);
const HEALTH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Poll `inspect` until the health status is `want`.
fn wait_for_health(d: &Daemon, id: &str, want: &str) {
    let deadline = std::time::Instant::now() + HEALTH_DEADLINE;
    while std::time::Instant::now() < deadline {
        let status = run_ok(d, &["inspect", "--format", "{{.State.Health.Status}}", id]);
        if status.trim() == want {
            return;
        }
        std::thread::sleep(HEALTH_POLL_STEP);
    }
    panic!("{id} never became {want}");
}

#[test]
fn given_a_health_check_when_the_probe_file_comes_and_goes_then_status_follows() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let image = image();
    let id = run_ok(
        &d,
        &[
            "run",
            "-d",
            "--health-cmd",
            "test -f /tmp/ok",
            "--health-interval",
            "1s",
            "--health-retries",
            "2",
            &image,
            "sleep",
            "120",
        ],
    );
    let id = id.trim();

    // Act
    let first = run_ok(&d, &["inspect", "--format", "{{.State.Health.Status}}", id]);
    run_ok(&d, &["exec", id, "touch", "/tmp/ok"]);
    wait_for_health(&d, id, "healthy");
    let listed = run_ok(&d, &["ps", "--filter", "health=healthy"]);
    run_ok(&d, &["exec", id, "rm", "/tmp/ok"]);
    wait_for_health(&d, id, "unhealthy");
    let streak = run_ok(
        &d,
        &["inspect", "--format", "{{.State.Health.FailingStreak}}", id],
    );

    // Assert
    assert_eq!(first.trim(), "starting");
    assert!(listed.contains("(healthy)"), "{listed}");
    assert!(streak.trim().parse::<u32>().unwrap() >= 2, "{streak}");
}
