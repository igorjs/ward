// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests against real microVMs.
//!
//! Each test starts a real `wardd`, pulls a real image, boots a real Linux
//! guest and drives it through the `ward` CLI, like a user would.
//!
//! Needs a hypervisor (KVM on Linux, Hypervisor.framework on macOS), network
//! access to a registry, and a build with `--features krunvm`. Set
//! `WARD_E2E_VM=1` to run them. Without it every test returns early, so the
//! normal `cargo test` stays hermetic. `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm

#![cfg(feature = "krunvm")]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const EXIT_POLL: Duration = Duration::from_millis(100);
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(10);
const LARGE_OUTPUT_BYTES: usize = 1_000_000;

/// Return early unless the operator opted in to real-VM tests.
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

fn field(stdout: &str, prefix: &str) -> String {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in:\n{stdout}"))
        .trim()
        .to_string()
}

fn run_ok(mut cmd: Command) -> String {
    let out = cmd.output().expect("run ward");
    assert!(
        out.status.success(),
        "command failed: {:?}\nstdout: {}\nstderr: {}",
        cmd,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

/// Create a sandbox and return its id.
fn create(d: &Daemon, extra: &[&str]) -> String {
    let mut cmd = d.cli();
    cmd.arg("create").args(extra).arg(image());
    let out = run_ok(cmd);
    assert_eq!(field(&out, "status: "), "running", "create output:\n{out}");
    field(&out, "id: ")
}

struct Finished {
    stdout: String,
    stderr: String,
    exit: i32,
}

/// Start a process and return its pid without waiting.
fn start(d: &Daemon, id: &str, workdir: Option<&str>, argv: &[&str]) -> String {
    let mut cmd = d.cli();
    cmd.args(["exec", id]);
    if let Some(dir) = workdir {
        cmd.args(["--workdir", dir]);
    }
    cmd.arg("--").args(argv);
    field(&run_ok(cmd), "pid: ")
}

/// Read a process's output until it exits.
fn collect(d: &Daemon, id: &str, pid: &str) -> Finished {
    let mut cmd = d.cli();
    cmd.args(["logs", id, pid]);
    let out = run_ok(cmd);
    let mut done = Finished {
        stdout: String::new(),
        stderr: String::new(),
        exit: i32::MIN,
    };
    for line in out.lines() {
        if let Some(text) = line.strip_prefix("stdout: ") {
            done.stdout.push_str(text);
            done.stdout.push('\n');
        } else if let Some(text) = line.strip_prefix("stderr: ") {
            done.stderr.push_str(text);
            done.stderr.push('\n');
        } else if let Some(code) = line.strip_prefix("exit: ") {
            done.exit = code.trim().parse().expect("exit code");
        }
    }
    assert_ne!(done.exit, i32::MIN, "no exit line in logs:\n{out}");
    done
}

fn exec(d: &Daemon, id: &str, argv: &[&str]) -> Finished {
    let pid = start(d, id, None, argv);
    collect(d, id, &pid)
}

fn sh(d: &Daemon, id: &str, script: &str) -> Finished {
    exec(d, id, &["sh", "-c", script])
}

/// Direct children of `parent` whose command line carries `needle`.
fn children_of(parent: u32, needle: &str) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &parent.to_string(), "-f", needle])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(EXIT_POLL);
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}

const SHIM_ARG: &str = "__ward-vm-shim";

#[test]
fn given_image_when_create_then_guest_boots_real_linux_kernel() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let kernel = exec(&d, &id, &["uname", "-sr"]);

    assert_eq!(kernel.exit, 0);
    assert!(
        kernel.stdout.starts_with("Linux "),
        "got {:?}",
        kernel.stdout
    );
}

#[test]
fn given_running_sandbox_when_exec_echo_then_stdout_and_zero_exit() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = exec(&d, &id, &["echo", "hello ward"]);

    assert_eq!((r.stdout.as_str(), r.exit), ("hello ward\n", 0));
}

#[test]
fn given_failing_command_when_exec_then_exit_code_is_reported() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = sh(&d, &id, "exit 3");

    assert_eq!(r.exit, 3);
}

#[test]
fn given_command_writing_stderr_when_exec_then_stderr_is_captured_separately() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = sh(&d, &id, "echo out; echo err >&2");

    assert_eq!(r.stdout, "out\n");
    assert_eq!(r.stderr, "err\n");
}

#[test]
fn given_env_at_create_when_exec_then_guest_sees_variable() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["-e", "WARD_E2E_VAR=hello"]);

    let r = sh(&d, &id, "printf %s \"$WARD_E2E_VAR\"");

    assert_eq!(r.stdout.trim(), "hello");
}

#[test]
fn given_workdir_when_exec_then_command_runs_there() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let pid = start(&d, &id, Some("/etc"), &["pwd"]);
    let r = collect(&d, &id, &pid);

    assert_eq!(r.stdout.trim(), "/etc");
}

#[test]
fn given_memory_and_cpus_when_create_then_guest_sees_them() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["--memory", "256", "--cpus", "2"]);

    let cpus = sh(&d, &id, "nproc");
    let mem_kb = sh(&d, &id, "awk '/MemTotal/ {print $2}' /proc/meminfo");

    assert_eq!(cpus.stdout.trim(), "2");
    let kb: u64 = mem_kb.stdout.trim().parse().expect("MemTotal");
    assert!((150_000..=300_000).contains(&kb), "MemTotal {kb} kB");
}

#[test]
fn given_proc_and_dev_when_inspected_then_guest_has_usable_pseudo_filesystems() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = sh(
        &d,
        &id,
        "test -r /proc/self/status && test -c /dev/null && echo ok",
    );

    assert_eq!(r.stdout.trim(), "ok");
}

#[test]
fn given_large_output_when_exec_then_nothing_is_lost() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let script = format!("head -c {LARGE_OUTPUT_BYTES} /dev/zero | tr '\\0' 'x' | wc -c");
    let r = sh(&d, &id, &script);

    assert_eq!(r.stdout.trim(), LARGE_OUTPUT_BYTES.to_string());
}

#[test]
fn given_process_reading_stdin_when_data_written_then_process_receives_it() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let pid = start(&d, &id, None, &["head", "-n", "1"]);
    run_ok({
        let mut c = d.cli();
        c.args(["stdin", &id, &pid, "ping from host\n"]);
        c
    });
    let r = collect(&d, &id, &pid);

    assert_eq!(r.stdout.trim(), "ping from host");
}

#[test]
fn given_long_running_process_when_killed_then_it_is_gone_from_the_guest() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    let pid = start(&d, &id, None, &["sleep", "300"]);
    let running = "ps | grep -q '[s]leep 300'";
    assert_eq!(
        sh(&d, &id, &format!("{running} && echo up")).stdout.trim(),
        "up"
    );

    run_ok({
        let mut c = d.cli();
        c.args(["kill", &id, &pid]);
        c
    });

    let probe = format!(
        "for i in 1 2 3 4 5 6 7 8 9 10; do {running} || {{ echo gone; exit 0; }}; sleep 1; done; echo alive"
    );
    assert_eq!(sh(&d, &id, &probe).stdout.trim(), "gone");
}

#[test]
fn given_two_sandboxes_when_one_writes_a_file_then_the_other_cannot_see_it() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let a = create(&d, &[]);
    let b = create(&d, &[]);

    assert_eq!(sh(&d, &a, "echo secret > /tmp/only-in-a").exit, 0);
    let in_a = sh(&d, &a, "cat /tmp/only-in-a");
    let in_b = sh(&d, &b, "test -e /tmp/only-in-a; echo $?");

    assert_eq!(in_a.stdout.trim(), "secret");
    assert_eq!(in_b.stdout.trim(), "1");
}

#[test]
fn given_guest_edits_an_image_file_when_another_sandbox_boots_then_it_sees_the_original() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let a = create(&d, &[]);
    assert_eq!(sh(&d, &a, "echo tampered >> /etc/hostname && sync").exit, 0);

    let b = create(&d, &[]);
    let in_b = sh(&d, &b, "grep -c tampered /etc/hostname; true");

    assert_eq!(in_b.stdout.trim(), "0");
}

#[test]
fn given_two_sandboxes_when_listed_then_both_run_concurrently() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let a = create(&d, &[]);
    let b = create(&d, &[]);

    let listing = run_ok({
        let mut c = d.cli();
        c.arg("list");
        c
    });

    for id in [&a, &b] {
        let row = listing.lines().find(|l| l.contains(id.as_str()));
        assert!(row.is_some_and(|r| r.contains("running")), "{listing}");
    }
    assert_eq!(children_of(d.pid(), SHIM_ARG).len(), 2);
}

#[test]
fn given_sandbox_when_removed_then_only_its_vm_process_stops() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let a = create(&d, &[]);
    let b = create(&d, &[]);
    assert_eq!(children_of(d.pid(), SHIM_ARG).len(), 2);

    run_ok({
        let mut c = d.cli();
        c.args(["remove", &a]);
        c
    });

    wait_until("one VM process to stop", ORPHAN_TIMEOUT, || {
        children_of(d.pid(), SHIM_ARG).len() == 1
    });
    assert_eq!(sh(&d, &b, "echo still-up").stdout.trim(), "still-up");
}

#[test]
fn given_running_vms_when_daemon_is_killed_then_no_vm_process_is_left_behind() {
    require_vm!();
    let mut d = Daemon::spawn_vm();
    create(&d, &[]);
    create(&d, &[]);
    let shims = children_of(d.pid(), SHIM_ARG);
    assert_eq!(shims.len(), 2);

    d.kill_hard();

    wait_until("VM processes to exit", ORPHAN_TIMEOUT, || {
        shims.iter().all(|p| !alive(*p))
    });
}

#[test]
fn given_default_egress_when_guest_connects_out_then_it_is_denied() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = sh(
        &d,
        &id,
        "wget -q -T 5 -O /dev/null http://1.1.1.1/ ; echo $?",
    );

    assert_ne!(r.stdout.trim(), "0");
}

#[test]
fn given_snapshot_when_restored_into_new_sandbox_then_files_are_present() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let src = create(&d, &[]);
    assert_eq!(sh(&d, &src, "echo kept > /root/note && sync").exit, 0);

    let snap = field(
        &run_ok({
            let mut c = d.cli();
            c.args(["snapshot", "create", &src]);
            c
        }),
        "snapshot_id: ",
    );
    let restored = field(
        &run_ok({
            let mut c = d.cli();
            c.args([
                "create",
                "--from-snapshot",
                &snap,
                "--source-sandbox",
                &src,
                &image(),
            ]);
            c
        }),
        "id: ",
    );

    let r = exec(&d, &restored, &["cat", "/root/note"]);
    assert_eq!(r.stdout.trim(), "kept");
}

#[test]
fn given_read_write_mount_when_guest_reads_and_writes_then_host_sees_the_changes() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let share = tempfile::tempdir().expect("share dir");
    std::fs::write(share.path().join("in.txt"), "from host").unwrap();
    let spec = format!("{}:/mnt/share", share.path().display());
    let id = create(&d, &["--mount", &spec]);

    let r = sh(
        &d,
        &id,
        "cat /mnt/share/in.txt; echo from guest > /mnt/share/out.txt",
    );

    assert_eq!(r.stdout.trim(), "from host");
    assert_eq!(
        std::fs::read_to_string(share.path().join("out.txt"))
            .unwrap()
            .trim(),
        "from guest"
    );
}

#[test]
fn given_read_only_mount_when_guest_writes_then_the_write_fails_and_host_is_unchanged() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let share = tempfile::tempdir().expect("share dir");
    std::fs::write(share.path().join("in.txt"), "original").unwrap();
    let spec = format!("{}:/mnt/ro:ro", share.path().display());
    let id = create(&d, &["--mount", &spec]);

    let r = sh(&d, &id, "echo changed > /mnt/ro/in.txt; echo rc=$?");

    assert_ne!(r.stdout.trim(), "rc=0");
    assert_eq!(
        std::fs::read_to_string(share.path().join("in.txt")).unwrap(),
        "original"
    );
}

/// First line of an HTTP response from `host`, or empty when unreachable.
fn http_status_line(d: &Daemon, id: &str, host: &str) -> String {
    let script = format!(
        "wget -q -T 15 -S -O /dev/null http://{host} 2>&1 | grep -m1 -o 'HTTP/[0-9.]* [0-9]*'; true"
    );
    sh(d, id, &script).stdout.trim().to_string()
}

#[test]
fn given_allowlist_egress_when_guest_requests_listed_host_then_it_is_reachable() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(
        &d,
        &["--egress", "allowlist", "--allow-domain", "github.com"],
    );

    let status = http_status_line(&d, &id, "github.com");

    assert!(
        status.starts_with("HTTP/"),
        "no HTTP response from github.com: {status:?}"
    );
}

#[test]
fn given_allowlist_egress_when_guest_requests_unlisted_host_then_it_is_blocked() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(
        &d,
        &["--egress", "allowlist", "--allow-domain", "github.com"],
    );

    let status = http_status_line(&d, &id, "api.github.com");

    assert_eq!(status, "", "unlisted host must not answer");
}

#[test]
fn given_open_egress_when_guest_targets_cloud_metadata_address_then_it_is_blocked() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["--egress", "open"]);

    let status = http_status_line(&d, &id, "169.254.169.254");

    assert_eq!(status, "");
}
