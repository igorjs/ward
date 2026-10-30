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
    out.trim().to_string()
}

struct Finished {
    stdout: String,
    stderr: String,
    exit: i32,
}

/// Start a process and return its pid without waiting.
fn start(d: &Daemon, id: &str, workdir: Option<&str>, argv: &[&str]) -> String {
    let mut cmd = d.cli();
    cmd.args(["exec", "-d", id]);
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
fn given_image_with_default_command_when_run_without_command_then_it_starts_and_exits_zero() {
    require_vm!();
    let d = Daemon::spawn_vm();

    let mut cmd = d.cli();
    cmd.args(["run", &image()]);
    let out = cmd.output().expect("run ward");

    assert_eq!(out.status.code(), Some(0), "stderr: {:?}", out.stderr);
}

#[test]
fn given_entrypoint_flag_when_run_with_arguments_then_the_entrypoint_runs_them() {
    require_vm!();
    let d = Daemon::spawn_vm();

    let mut cmd = d.cli();
    cmd.args([
        "run",
        "--entrypoint",
        "echo",
        &image(),
        "from",
        "entrypoint",
    ]);
    let out = run_ok(cmd);

    assert_eq!(out, "from entrypoint\n");
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

const OOM_MEMORY: &str = "128m";
const OOM_EXIT_CODE: i64 = 137;
const OOM_DEADLINE: Duration = Duration::from_secs(120);
/// Doubles a shell variable until the guest kernel kills the process.
const ALLOCATE_FOREVER: &str = "x=0123456789; while :; do x=$x$x; done";

#[test]
fn given_a_memory_hungry_main_process_when_the_guest_runs_out_then_inspect_reports_oom() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let mut cmd = d.cli();
    cmd.args([
        "run",
        "-d",
        "-m",
        OOM_MEMORY,
        &image(),
        "sh",
        "-c",
        ALLOCATE_FOREVER,
    ]);
    let id = run_ok(cmd).trim().to_string();

    wait_until("the sandbox to exit", OOM_DEADLINE, || {
        listed(&d, &id).is_some_and(|row| row.to_lowercase().contains("exited"))
    });

    let mut inspect = d.cli();
    inspect.args(["inspect", &id]);
    let found: serde_json::Value = serde_json::from_str(&run_ok(inspect)).expect("inspect json");
    let state = &found[0]["State"];
    assert_eq!(state["OOMKilled"], true, "{state}");
    assert_eq!(state["Reason"], "oom", "{state}");
    assert_eq!(state["ExitCode"], OOM_EXIT_CODE, "{state}");
}

#[test]
fn given_memory_and_cpus_when_create_then_guest_sees_them() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["--memory", "256m", "--cpus", "2"]);

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

    // The bytes themselves must cross the agent, vsock and daemon path.
    let script = format!("head -c {LARGE_OUTPUT_BYTES} /dev/zero | tr '\\0' 'x'");
    let r = sh(&d, &id, &script);

    // The CLI adds one trailing newline.
    assert_eq!(r.stdout.trim_end().len(), LARGE_OUTPUT_BYTES);
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
        c.args(["kill", &id, "--pid", &pid]);
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
        c.args(["remove", "-f", &a]);
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

fn lifecycle(d: &Daemon, verb: &str, id: &str) {
    let mut cmd = d.cli();
    cmd.args([verb, id]);
    run_ok(cmd);
}

#[test]
fn given_running_sandbox_with_file_when_stopped_and_started_then_file_has_identical_bytes() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(
        sh(&d, &id, "printf 'kept\\000bytes' > /root/note && sync").exit,
        0
    );

    lifecycle(&d, "stop", &id);
    wait_until("the VM process to stop", ORPHAN_TIMEOUT, || {
        children_of(d.pid(), SHIM_ARG).is_empty()
    });
    lifecycle(&d, "start", &id);

    let read = sh(&d, &id, "od -An -c /root/note");
    assert_eq!(read.exit, 0);
    assert_eq!(
        read.stdout.split_whitespace().collect::<Vec<_>>(),
        ["k", "e", "p", "t", "\\0", "b", "y", "t", "e", "s"]
    );
}

#[test]
fn given_running_sandbox_when_started_then_exactly_one_vm_process_exists() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(children_of(d.pid(), SHIM_ARG).len(), 1);

    lifecycle(&d, "start", &id);

    wait_until("one VM process", ORPHAN_TIMEOUT, || {
        children_of(d.pid(), SHIM_ARG).len() == 1
    });
    assert_eq!(sh(&d, &id, "echo up").stdout.trim(), "up");
}

#[test]
fn given_stopped_sandbox_when_stopped_again_then_it_succeeds_and_stays_stopped() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    lifecycle(&d, "stop", &id);

    lifecycle(&d, "stop", &id);

    let listing = run_ok({
        let mut c = d.cli();
        c.arg("list");
        c
    });
    let row = listing.lines().find(|l| l.contains(id.as_str()));
    assert!(row.is_some_and(|r| r.contains("stopped")), "{listing}");
    assert!(children_of(d.pid(), SHIM_ARG).is_empty());
}

/// Run the CLI and return its exit success plus stderr.
fn try_cli(d: &Daemon, args: &[&str]) -> (bool, String) {
    let out = d.cli().args(args).output().expect("run ward");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn listed(d: &Daemon, id: &str) -> Option<String> {
    let mut cmd = d.cli();
    cmd.arg("list");
    run_ok(cmd)
        .lines()
        .find(|l| l.contains(id))
        .map(str::to_string)
}

#[test]
fn given_running_sandbox_with_file_when_restarted_then_it_runs_and_the_file_is_there() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(sh(&d, &id, "echo kept > /root/note && sync").exit, 0);

    lifecycle(&d, "restart", &id);

    assert!(listed(&d, &id).is_some_and(|r| r.contains("running")));
    assert_eq!(sh(&d, &id, "cat /root/note").stdout.trim(), "kept");
}

#[test]
fn given_running_sandbox_when_rm_force_then_the_sandbox_and_its_vm_are_gone() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let mut cmd = d.cli();
    cmd.args(["rm", "-f", &id]);
    run_ok(cmd);

    assert_eq!(listed(&d, &id), None);
    wait_until("the VM process to stop", ORPHAN_TIMEOUT, || {
        children_of(d.pid(), SHIM_ARG).is_empty()
    });
}

#[test]
fn given_running_sandbox_when_rm_then_it_fails_and_the_sandbox_is_still_running() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let (success, stderr) = try_cli(&d, &["rm", &id]);

    assert!(!success);
    assert!(
        stderr.contains("cannot remove a running sandbox"),
        "{stderr}"
    );
    assert!(listed(&d, &id).is_some_and(|r| r.contains("running")));
    assert_eq!(sh(&d, &id, "echo up").stdout.trim(), "up");
}

#[test]
fn given_stopped_and_running_sandbox_when_pruned_then_only_the_stopped_one_is_removed() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let stopped = create(&d, &[]);
    let running = create(&d, &[]);
    lifecycle(&d, "stop", &stopped);

    let mut cmd = d.cli();
    cmd.arg("prune");
    let printed = run_ok(cmd);

    assert_eq!(printed.trim(), stopped);
    assert_eq!(listed(&d, &stopped), None);
    assert!(listed(&d, &running).is_some_and(|r| r.contains("running")));
}

#[test]
fn given_default_egress_when_guest_connects_to_the_host_then_nothing_arrives() {
    require_vm!();
    // A listener on the host's loopback is the one target a deny-mode guest
    // must not reach, whatever the guest's tools do with the failure.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind host listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = sh(
        &d,
        &id,
        &format!("wget -q -T 5 -O /dev/null http://127.0.0.1:{port}/ ; echo $?"),
    );

    assert_ne!(r.stdout.trim(), "0");
    assert!(
        listener.accept().is_err(),
        "the guest reached a host socket in deny mode"
    );
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
    let restored = run_ok({
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
    })
    .trim()
    .to_string();

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
    let id = create(&d, &["--volume", &spec]);

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
    let id = create(&d, &["--volume", &spec]);

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
        &[
            "--egress",
            "allowlist",
            "--allow-domain",
            "archive.ubuntu.com",
        ],
    );

    let status = http_status_line(&d, &id, "archive.ubuntu.com");

    assert!(
        status.starts_with("HTTP/"),
        "no HTTP response from archive.ubuntu.com: {status:?}"
    );
}

#[test]
fn given_allowlist_egress_when_guest_requests_unlisted_host_then_it_is_blocked() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(
        &d,
        &[
            "--egress",
            "allowlist",
            "--allow-domain",
            "archive.ubuntu.com",
        ],
    );

    let status = http_status_line(&d, &id, "security.ubuntu.com");

    assert_eq!(status, "", "unlisted host must not answer");
}

#[test]
fn given_allowlist_egress_when_guest_is_blocked_then_egress_suggest_lists_the_host() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(
        &d,
        &[
            "--egress",
            "allowlist",
            "--allow-domain",
            "archive.ubuntu.com",
        ],
    );
    http_status_line(&d, &id, "security.ubuntu.com");

    let mut cmd = d.cli();
    cmd.args(["egress", "suggest", &id]);
    let suggested = run_ok(cmd);

    assert!(
        suggested
            .lines()
            .any(|l| l == "--allow-domain security.ubuntu.com"),
        "blocked host missing from suggestions:\n{suggested}"
    );
}

#[test]
fn given_open_egress_when_guest_targets_cloud_metadata_address_then_it_is_blocked() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["--egress", "open"]);

    let status = http_status_line(&d, &id, "169.254.169.254");

    assert_eq!(status, "");
}

#[test]
fn given_a_running_guest_process_when_top_then_one_row_has_its_pid_and_command() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    start(&d, &id, None, &["sleep", "31337"]);
    let guest_pid = sh(&d, &id, "pgrep -f '[s]leep 31337'")
        .stdout
        .trim()
        .to_string();

    let mut cmd = d.cli();
    cmd.args(["top", &id]);
    let out = run_ok(cmd);

    let header = out.lines().next().expect("header");
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"]
    );
    let rows: Vec<Vec<&str>> = out
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.get(1) == Some(&guest_pid.as_str()))
        .collect();
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0][7..], ["sleep", "31337"]);
}

const GUEST_BUSY_SECONDS: u64 = 2;

/// One `ward stats --no-stream` reading: a counter from its JSON object.
fn stat(d: &Daemon, id: &str, key: &str) -> u64 {
    let mut cmd = d.cli();
    cmd.args(["--json", "stats", "--no-stream", id]);
    let value: serde_json::Value = serde_json::from_str(run_ok(cmd).trim()).expect("stats json");
    value[key]
        .as_u64()
        .unwrap_or_else(|| panic!("no {key} in {value}"))
}

#[test]
fn given_a_guest_busy_loop_when_stats_read_twice_then_cpu_time_grew_within_wall_time_times_cpus() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    let started = Instant::now();
    let before = stat(&d, &id, "CPUNanos");

    let busy = format!(
        "end=$(( $(date +%s) + {GUEST_BUSY_SECONDS} )); while [ $(date +%s) -lt $end ]; do :; done"
    );
    assert_eq!(sh(&d, &id, &busy).exit, 0);
    let grew = stat(&d, &id, "CPUNanos") - before;

    let ceiling = started.elapsed().as_nanos() * u128::from(stat(&d, &id, "OnlineCPUs"));
    assert!(grew > 0, "cpu time did not grow");
    assert!(
        u128::from(grew) <= ceiling,
        "{grew} ns of cpu over {ceiling}"
    );
}

const USER_UID: &str = "1000";
const USER_EXTRA_GID: &str = "1010";
/// Adds uid 1000 to the guest's user database, in a group of its own and in
/// one more that lists it.
const ADD_USER: &str = "echo 'dev:x:1000:1000::/home/dev:/bin/sh' >> /etc/passwd; \
                        echo 'dev:x:1000:' >> /etc/group; \
                        echo 'extra:x:1010:dev' >> /etc/group";
const EXIT_NOT_INVOKABLE: i32 = 126;

/// Run `argv` through `ward exec -u USER` and return its output and exit code.
fn exec_as(d: &Daemon, id: &str, user: &str, argv: &[&str]) -> Finished {
    let out = d
        .cli()
        .args(["exec", "-u", user, id, "--"])
        .args(argv)
        .output()
        .expect("run ward");
    Finished {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        exit: out.status.code().expect("exit code"),
    }
}

#[test]
fn given_user_1000_when_run_then_id_u_prints_1000() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let mut cmd = d.cli();
    cmd.args(["run", "--rm", "-u", "1000:1000", &image(), "id", "-u"]);

    let out = run_ok(cmd);

    assert_eq!(out.trim(), USER_UID);
}

#[test]
fn given_user_1000_1000_when_exec_id_u_then_it_prints_1000() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = exec_as(&d, &id, "1000:1000", &["id", "-u"]);

    assert_eq!((r.stdout.trim(), r.exit), (USER_UID, 0));
}

#[test]
fn given_uid_gid_when_exec_id_g_then_only_that_group_is_listed() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(sh(&d, &id, ADD_USER).exit, 0);

    let r = exec_as(&d, &id, "1000:1000", &["id", "-G"]);

    assert_eq!(r.stdout.trim(), USER_UID, "stderr: {}", r.stderr);
}

#[test]
fn given_bare_user_name_when_exec_id_g_then_guest_group_memberships_are_listed() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(sh(&d, &id, ADD_USER).exit, 0);

    let r = exec_as(&d, &id, "dev", &["id", "-G"]);

    let mut groups: Vec<&str> = r.stdout.split_whitespace().collect();
    groups.sort_unstable();
    assert_eq!(groups, [USER_UID, USER_EXTRA_GID], "stderr: {}", r.stderr);
}

#[test]
fn given_a_root_only_workdir_when_exec_as_user_then_it_fails_after_the_drop() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(sh(&d, &id, "mkdir -m 0700 /rootonly").exit, 0);

    let out = d
        .cli()
        .args([
            "exec",
            "-u",
            "1000:1000",
            "-w",
            "/rootonly",
            &id,
            "--",
            "pwd",
        ])
        .output()
        .expect("run ward");

    assert_eq!(out.status.code(), Some(EXIT_NOT_INVOKABLE));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
}

#[test]
fn given_user_name_when_exec_then_home_comes_from_passwd() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);
    assert_eq!(sh(&d, &id, ADD_USER).exit, 0);

    let r = exec_as(&d, &id, "dev", &["sh", "-c", "echo $HOME"]);

    assert_eq!(r.stdout.trim(), "/home/dev");
}

#[test]
fn given_unknown_user_when_exec_then_exit_126_with_dockers_wording() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &[]);

    let r = exec_as(&d, &id, "ghost", &["id", "-u"]);

    assert_eq!(r.exit, EXIT_NOT_INVOKABLE);
    assert!(
        r.stderr
            .contains("unable to find user ghost: no matching entries in passwd file"),
        "stderr: {}",
        r.stderr
    );
}

const GUEST_REPLY: &str = "ward-guest-reply";
const DIAL_DEADLINE: Duration = Duration::from_secs(30);
const HTTP_PORT: u16 = 80;

/// Guest script that answers every connection on `port` with `GUEST_REPLY`.
fn reply_server(port: u16) -> String {
    format!("while true; do printf '{GUEST_REPLY}' | nc -l -p {port}; done")
}

/// `ward port ID` as (container port, host port) pairs.
fn published(d: &Daemon, id: &str) -> Vec<(u16, u16)> {
    let mut cmd = d.cli();
    cmd.args(["port", id]);
    run_ok(cmd)
        .lines()
        .map(|line| {
            let (container, address) = line.split_once("/tcp -> 127.0.0.1:").expect(line);
            (container.parse().unwrap(), address.parse().unwrap())
        })
        .collect()
}

/// Dial the host port until the guest answers, then return what it sent.
fn dial_reply(port: u16) -> String {
    use std::io::Read;
    let deadline = Instant::now() + DIAL_DEADLINE;
    loop {
        let mut reply = String::new();
        let read = std::net::TcpStream::connect(("127.0.0.1", port))
            .and_then(|mut stream| stream.read_to_string(&mut reply));
        if read.is_ok() && !reply.is_empty() {
            return reply;
        }
        assert!(Instant::now() < deadline, "no reply on host port {port}");
        std::thread::sleep(EXIT_POLL);
    }
}

fn refused(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused)
}

#[test]
fn given_any_host_port_and_a_guest_server_when_the_host_port_is_dialed_then_the_reply_arrives() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["-p", "0:80"]);
    start(&d, &id, None, &["sh", "-c", &reply_server(HTTP_PORT)]);

    let mappings = published(&d, &id);

    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].0, HTTP_PORT);
    assert_eq!(dial_reply(mappings[0].1), GUEST_REPLY);
}

#[test]
fn given_a_published_port_when_stopped_and_started_then_it_is_refused_then_serves_on_the_same_port()
{
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["-p", "0:80"]);
    let port = published(&d, &id)[0].1;

    lifecycle(&d, "stop", &id);
    wait_until("the host port to close", DIAL_DEADLINE, || refused(port));
    lifecycle(&d, "start", &id);
    start(&d, &id, None, &["sh", "-c", &reply_server(HTTP_PORT)]);

    assert_eq!(published(&d, &id)[0].1, port);
    assert_eq!(dial_reply(port), GUEST_REPLY);
}

const SECRET_VALUE: &str = "hunter2-correct-horse-battery-staple";
const SECRET_NAME: &str = "db-password";
const SECRET_USER: &str = "1000:1000";
const SECRET_MODE_AND_OWNER: &str = "400:1000";

fn secret_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join(SECRET_NAME);
    std::fs::write(&path, SECRET_VALUE).expect("write secret file");
    path
}

fn read_secret_script() -> String {
    format!("cat /run/secrets/{SECRET_NAME}; stat -c '%a:%u' /run/secrets/{SECRET_NAME}")
}

#[test]
fn given_a_secret_when_run_then_the_guest_reads_it_as_a_read_only_file_of_the_user() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = secret_file(&dir);

    let mut cmd = d.cli();
    cmd.args(["run", "--user", SECRET_USER, "--secret"])
        .arg(&file)
        .arg(image())
        .args(["sh", "-c", &read_secret_script()]);
    let out = run_ok(cmd);

    assert!(out.contains(SECRET_VALUE), "got {out:?}");
    assert!(out.contains(SECRET_MODE_AND_OWNER), "got {out:?}");
}

#[test]
fn given_a_secret_when_exec_then_the_guest_reads_it_and_inspect_never_shows_it() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let dir = tempfile::tempdir().expect("tempdir");
    let file = secret_file(&dir);
    let id = create(&d, &[]);

    let mut cmd = d.cli();
    cmd.args(["exec", "--user", SECRET_USER, "--secret"])
        .arg(&file)
        .arg(&id)
        .args(["sh", "-c", &read_secret_script()]);
    let out = run_ok(cmd);
    let mut inspect = d.cli();
    inspect.args(["inspect", &id]);
    let inspected = run_ok(inspect);

    assert!(out.contains(SECRET_VALUE), "got {out:?}");
    assert!(out.contains(SECRET_MODE_AND_OWNER), "got {out:?}");
    assert!(
        !inspected.contains(SECRET_VALUE),
        "inspect leaked the secret"
    );
}

const PIDS_LIMIT: &str = "20";
const FORK_ATTEMPTS: u32 = 100;
const SLEEPER_SECONDS: u32 = 2;
const ALLOC_MEMORY: &str = "128m";

#[test]
fn given_a_pids_limit_when_a_fork_loop_runs_then_forks_fail_and_the_host_stays_responsive() {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["--pids-limit", PIDS_LIMIT]);
    let script = format!(
        "i=0; while [ $i -lt {FORK_ATTEMPTS} ]; do sleep {SLEEPER_SECONDS} & i=$((i+1)); done; wait"
    );

    let storm = sh(&d, &id, &script);
    let after = sh(&d, &id, "echo still-here");

    assert!(
        storm.stderr.contains("fork") || storm.stderr.contains("temporarily unavailable"),
        "expected fork failures, got stderr {:?}",
        storm.stderr
    );
    assert_eq!(after.stdout.trim(), "still-here");
    assert_eq!(after.exit, 0);
}

#[test]
fn given_a_memory_limit_when_a_process_allocates_without_bound_then_it_is_killed_and_the_host_stays_responsive()
 {
    require_vm!();
    let d = Daemon::spawn_vm();
    let id = create(&d, &["-m", ALLOC_MEMORY]);

    let hog = sh(&d, &id, "x=a; while :; do x=\"$x$x\"; done");
    let after = sh(&d, &id, "echo still-here");

    assert_ne!(hog.exit, 0, "the allocation loop must not finish cleanly");
    assert_eq!(after.stdout.trim(), "still-here");
}
