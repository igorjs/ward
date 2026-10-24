// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Signal handling of the `ward` CLI against a real guest.
//!
//! The stub backend cannot hold a live process, so these paths only run on a
//! real microVM: Ctrl-C in `logs -f`, a signal during `run --rm -it`, and
//! `run -t` without `-i`. Needs `--features krunvm` and `WARD_E2E_VM=1`;
//! `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_signals

#![cfg(feature = "krunvm")]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Daemon;
use rustix::process::{Pid, Signal, kill_process};

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const POLL_STEP: Duration = Duration::from_millis(100);
const WAIT_DEADLINE: Duration = Duration::from_secs(60);
/// Time for the CLI to register its signal handlers after the guest is up.
const SIGNAL_SETTLE: Duration = Duration::from_secs(1);
const SIGNAL_EXIT_BASE: i32 = 128;
const SIGINT_EXIT: i32 = SIGNAL_EXIT_BASE + 2;
const SIGTERM_EXIT: i32 = SIGNAL_EXIT_BASE + 15;
/// Exit code the guest script picks when it handles SIGINT itself.
const GUEST_HANDLED_EXIT: i32 = 42;
const READY_LINE: &str = "ready";
const DRAIN_BUFFER_BYTES: usize = 256;
const SLEEP_COMMAND: &str = "sleep 300";
/// Matches the guest's `sleep 300` without matching the probe's own shell.
const SLEEP_PROBE: &str = "ps | grep -q '[s]leep 300'";
/// Prints a marker, then loops in short sleeps so the trap fires promptly.
const TRAPPING_SCRIPT: &str = "trap 'exit 42' INT; echo ready; while :; do sleep 1; done";
const SHIM_ARG: &str = "__ward-vm-shim";

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

fn run_ok(mut cmd: Command) -> String {
    let out = cmd.output().expect("run ward");
    assert!(
        out.status.success(),
        "command failed: {cmd:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT_DEADLINE;
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(POLL_STEP);
    }
    panic!("timed out after {WAIT_DEADLINE:?} waiting for {what}");
}

fn is_listed(d: &Daemon, name: &str) -> bool {
    let mut cmd = d.cli();
    cmd.args(["ps", "-a"]);
    run_ok(cmd).lines().any(|l| l.contains(name))
}

/// Whether the guest of sandbox `name` still runs `sleep 300`.
fn guest_sleeps(d: &Daemon, name: &str) -> bool {
    d.cli()
        .args(["exec", name, "sh", "-c", SLEEP_PROBE])
        .status()
        .expect("probe guest")
        .success()
}

fn vm_processes(d: &Daemon) -> usize {
    let out = Command::new("pgrep")
        .args(["-P", &d.pid().to_string(), "-f", SHIM_ARG])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .count()
}

fn send(child: &Child, signal: Signal) {
    let raw = i32::try_from(child.id()).expect("pid fits i32");
    kill_process(Pid::from_raw(raw).expect("non-zero pid"), signal).expect("send signal");
}

fn wait_for_exit(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + WAIT_DEADLINE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll cli") {
            return status.code();
        }
        std::thread::sleep(POLL_STEP);
    }
    let _ = child.kill();
    panic!("the CLI did not exit within {WAIT_DEADLINE:?} of the signal");
}

/// Block until the child's stdout shows a line holding `needle`.
fn wait_for_output(child: &mut Child, needle: &'static str) {
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains(needle) {
                let _ = tx.send(());
                return;
            }
        }
    });
    rx.recv_timeout(WAIT_DEADLINE)
        .unwrap_or_else(|_| panic!("no {needle:?} line from the CLI within {WAIT_DEADLINE:?}"));
}

/// A pty whose slave side the CLI uses as its terminal.
struct Terminal {
    slave_path: rustix::ffi::CString,
    drain: Option<std::thread::JoinHandle<()>>,
}

impl Terminal {
    fn open() -> Self {
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let slave_path = ptsname(&master, Vec::new()).expect("ptsname");
        // The terminal flushes its output on every mode change, so something
        // must read the master side or the CLI blocks.
        let drain = std::thread::spawn(move || {
            let mut buf = [0u8; DRAIN_BUFFER_BYTES];
            while rustix::io::read(&master, &mut buf).is_ok_and(|n| n > 0) {}
        });
        Self {
            slave_path,
            drain: Some(drain),
        }
    }

    fn slave(&self) -> Stdio {
        use rustix::fs::{Mode, OFlags, open};

        let slave = open(
            self.slave_path.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        );
        Stdio::from(slave.expect("open slave"))
    }

    /// Wait for the drain thread, which ends once every slave is closed.
    fn finish(mut self) {
        self.drain
            .take()
            .expect("drain")
            .join()
            .expect("drain thread");
    }
}

#[test]
fn given_logs_f_when_ctrl_c_arrives_then_it_exits_130_and_the_guest_keeps_running() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let name = "logs-follow";
    run_ok({
        let mut c = d.cli();
        c.args(["run", "-d", "--name", name, &image()]).args([
            "sh",
            "-c",
            &format!("echo {READY_LINE}; exec {SLEEP_COMMAND}"),
        ]);
        c
    });
    let mut follower = d
        .cli()
        .args(["logs", "-f", name])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn logs -f");
    wait_for_output(&mut follower, READY_LINE);
    std::thread::sleep(SIGNAL_SETTLE);

    // Act
    send(&follower, Signal::INT);
    let code = wait_for_exit(&mut follower);

    // Assert
    assert_eq!(code, Some(SIGINT_EXIT));
    assert!(is_listed(&d, name), "logs -f must not remove the sandbox");
    assert!(guest_sleeps(&d, name), "logs -f must not stop the guest");
}

fn run_rm_it_signalled(signal: Signal, expected_exit: i32) {
    // Arrange
    let d = Daemon::spawn_vm();
    let name = "run-rm-it";
    let terminal = Terminal::open();
    let mut cli = d
        .cli()
        .args(["run", "--rm", "-it", "--name", name, &image()])
        .args(["sh", "-c", &format!("exec {SLEEP_COMMAND}")])
        .stdin(terminal.slave())
        .stdout(terminal.slave())
        .stderr(terminal.slave())
        .spawn()
        .expect("spawn run --rm -it");
    wait_until("the guest process to start", || guest_sleeps(&d, name));
    std::thread::sleep(SIGNAL_SETTLE);

    // Act
    send(&cli, signal);
    let code = wait_for_exit(&mut cli);
    terminal.finish();

    // Assert
    assert_eq!(code, Some(expected_exit));
    wait_until("the sandbox to be removed", || !is_listed(&d, name));
    wait_until("the VM process to stop", || vm_processes(&d) == 0);
}

#[test]
fn given_run_rm_it_when_sigint_arrives_then_it_exits_130_and_the_sandbox_is_removed() {
    require_vm!();
    run_rm_it_signalled(Signal::INT, SIGINT_EXIT);
}

#[test]
fn given_run_rm_it_when_sigterm_arrives_then_it_exits_143_and_the_sandbox_is_removed() {
    require_vm!();
    run_rm_it_signalled(Signal::TERM, SIGTERM_EXIT);
}

#[test]
fn given_run_t_without_i_when_sigint_arrives_then_the_guest_handles_it_and_the_sandbox_is_removed()
{
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let name = "run-t-only";
    let mut cli = d
        .cli()
        .args(["run", "--rm", "-t", "--name", name, &image()])
        .args(["sh", "-c", TRAPPING_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn run -t");
    wait_for_output(&mut cli, READY_LINE);
    std::thread::sleep(SIGNAL_SETTLE);

    // Act
    send(&cli, Signal::INT);
    let code = wait_for_exit(&mut cli);

    // Assert: without -i the CLI relays the signal, so the guest decides the code.
    assert_eq!(code, Some(GUEST_HANDLED_EXIT));
    wait_until("the sandbox to be removed", || !is_listed(&d, name));
}
