// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! One host process per microVM.
//!
//! Apple's Hypervisor.framework allows a single VM per process, and
//! `krun_start_enter` never hands the process back while the guest runs.
//! So each sandbox boots inside a short-lived child: the ward binary
//! re-executes itself with [`SHIM_ARG`], reads a [`VmSpec`] from stdin and
//! enters the VM. Killing the child stops the VM and frees everything it
//! held, with no libkrun state left in the daemon.
//!
//! Every ward binary that can host sandboxes must call [`run_if_requested`]
//! first thing in `main`.

use std::ffi::{CString, OsStr};
use std::io::{BufRead, Read};
use std::os::fd::{AsFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, watch};

use super::krun_ffi;

/// First argument that turns a ward binary into a VM host process.
pub const SHIM_ARG: &str = "__ward-vm-shim";

/// Exit code when the spec could not be read or parsed.
const EXIT_BAD_SPEC: i32 = 64;
/// Exit code when libkrun refused to configure or start the VM.
const EXIT_BOOT_FAILED: i32 = 70;
/// Exit code when the parent process went away.
const EXIT_PARENT_GONE: i32 = 71;

/// How long [`ShimProcess::stop`] waits for the child after killing it.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// libkrun log level 1 reports errors only.
const KRUN_LOG_LEVEL_ERROR: u32 = 1;

/// Passed to `krun_add_vsock` to leave out TSI, libkrun's socket proxy.
/// With TSI on, guest sockets reach the host's network and bypass egress policy.
const VSOCK_TSI_NONE: u32 = 0;

/// virtiofs shm window size 0 keeps libkrun's default (no DAX window).
const VIRTIOFS_DEFAULT_SHM: u64 = 0;

/// A host directory shared into the guest over virtiofs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    pub tag: String,
    pub path: PathBuf,
    pub read_only: bool,
}

/// The virtio-net device: the guest end of a datagram socketpair that the
/// shim inherits as `fd`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetSpec {
    pub fd: RawFd,
    pub mac: [u8; 6],
}

/// libkrun takes the CPU count as one byte.
const MAX_VCPUS: u32 = 255;

/// The vCPU count as libkrun takes it.
fn vcpus(cpus: u32) -> Result<u8, String> {
    u8::try_from(cpus).map_err(|_| format!("cpus={cpus} exceeds libkrun's limit of {MAX_VCPUS}"))
}

/// Everything the shim needs to configure and boot one microVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub rootfs: PathBuf,
    /// 0 keeps libkrun's default CPU and memory sizing.
    pub cpus: u32,
    pub memory_mb: u32,
    pub exec_path: String,
    /// Arguments after the program name, passed to the guest entry process.
    pub exec_args: Vec<String>,
    pub agent_port: u32,
    pub agent_socket: PathBuf,
    pub mounts: Vec<MountSpec>,
    pub net: Option<NetSpec>,
}

/// Turn this process into a VM host when started with [`SHIM_ARG`].
/// Returns normally in every other case. Never returns in shim mode.
pub fn run_if_requested() {
    let mut args = std::env::args_os();
    let _program = args.next();
    if args.next().as_deref() != Some(OsStr::new(SHIM_ARG)) {
        return;
    }
    std::process::exit(shim_main());
}

fn shim_main() -> i32 {
    let mut stdin = std::io::stdin().lock();
    let mut line = String::new();
    if let Err(e) = stdin.read_line(&mut line) {
        eprintln!("ward vm shim: read spec: {e}");
        return EXIT_BAD_SPEC;
    }
    let spec: VmSpec = match serde_json::from_str(&line) {
        Ok(spec) => spec,
        Err(e) => {
            eprintln!("ward vm shim: parse spec: {e}");
            return EXIT_BAD_SPEC;
        }
    };
    drop(stdin);

    // The parent keeps our stdin open for the VM's whole life. EOF means the
    // parent exited or dropped us, so stop instead of leaving an orphan VM.
    std::thread::spawn(|| {
        let mut sink = Vec::new();
        let _ = std::io::stdin().lock().read_to_end(&mut sink);
        std::process::exit(EXIT_PARENT_GONE);
    });

    match boot(&spec) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ward vm shim: {e}");
            EXIT_BOOT_FAILED
        }
    }
}

fn check(ret: i32, what: &str) -> Result<u32, String> {
    u32::try_from(ret).map_err(|_| format!("{what} failed: errno {}", -ret))
}

fn cstring(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|e| format!("invalid string {value:?}: {e}"))
}

fn path_cstring(path: &Path) -> Result<CString, String> {
    cstring(&path.to_string_lossy())
}

/// Configure the libkrun context from `spec` and enter the VM. Returns only
/// when libkrun fails or the guest ends and libkrun hands control back.
fn boot(spec: &VmSpec) -> Result<(), String> {
    krun_ffi::ensure_loaded().map_err(|e| format!("load embedded libkrun: {e}"))?;
    let root = path_cstring(&spec.rootfs)?;
    let exec_path = cstring(&spec.exec_path)?;
    let socket = path_cstring(&spec.agent_socket)?;
    // libkrun wants null-terminated pointer arrays. argv excludes the
    // program name; envp is empty.
    let exec_args = spec
        .exec_args
        .iter()
        .map(|a| cstring(a))
        .collect::<Result<Vec<_>, _>>()?;
    let mut argv: Vec<*const std::ffi::c_char> = exec_args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let empty: [*const std::ffi::c_char; 1] = [std::ptr::null()];

    // SAFETY (whole block): every pointer passed below comes from a live
    // CString or array in this frame, and `ctx` is the id libkrun returned.
    unsafe {
        krun_ffi::krun_set_log_level(KRUN_LOG_LEVEL_ERROR);
        let ctx = check(krun_ffi::krun_create_ctx(), "krun_create_ctx")?;

        if let Some((cpus, memory_mb)) = super::krunvm::vm_config(spec.cpus, spec.memory_mb) {
            check(
                krun_ffi::krun_set_vm_config(ctx, vcpus(cpus)?, memory_mb),
                "krun_set_vm_config",
            )?;
        }
        check(krun_ffi::krun_set_root(ctx, root.as_ptr()), "krun_set_root")?;

        // The implicit vsock turns TSI on. Replace it with a plain vsock so the
        // agent port is the only channel between guest and host.
        check(
            krun_ffi::krun_disable_implicit_vsock(ctx),
            "krun_disable_implicit_vsock",
        )?;
        check(
            krun_ffi::krun_add_vsock(ctx, VSOCK_TSI_NONE),
            "krun_add_vsock",
        )?;

        if let Some(net) = &spec.net {
            krun_ffi::set_net_unixgram(ctx, net.fd, net.mac)?;
        }
        for mount in &spec.mounts {
            let tag = cstring(&mount.tag)?;
            let path = path_cstring(&mount.path)?;
            check(
                krun_ffi::krun_add_virtiofs3(
                    ctx,
                    tag.as_ptr(),
                    path.as_ptr(),
                    VIRTIOFS_DEFAULT_SHM,
                    mount.read_only,
                ),
                "krun_add_virtiofs3",
            )?;
        }

        check(
            krun_ffi::krun_set_exec(ctx, exec_path.as_ptr(), argv.as_ptr(), empty.as_ptr()),
            "krun_set_exec",
        )?;
        // `listen = true`: the guest agent listens on this port and the host
        // starts the connection. libkrun creates the Unix socket and forwards
        // each host connection to the guest.
        check(
            krun_ffi::krun_add_vsock_port2(ctx, spec.agent_port, socket.as_ptr(), true),
            "krun_add_vsock_port2",
        )?;

        check(krun_ffi::krun_start_enter(ctx), "krun_start_enter")?;
    }
    Ok(())
}

/// How a VM host process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimExit {
    /// Exit code, when the process exited by itself.
    pub code: Option<i32>,
    /// Signal that ended the process, when one did.
    pub signal: Option<i32>,
    /// How long the process lived.
    pub ran_for: Duration,
}

impl ShimExit {
    fn of(status: std::io::Result<std::process::ExitStatus>, ran_for: Duration) -> Self {
        use std::os::unix::process::ExitStatusExt;
        match status {
            Ok(status) => Self {
                code: status.code(),
                signal: status.signal(),
                ran_for,
            },
            Err(e) => {
                tracing::warn!("cannot read how the VM host process ended: {e}");
                Self {
                    code: None,
                    signal: None,
                    ran_for,
                }
            }
        }
    }
}

/// Handle to a running VM host process.
///
/// A task owns the child and waits on it, so its exit is seen when it
/// happens and the child is always reaped.
#[derive(Debug)]
pub struct ShimProcess {
    pid: u32,
    /// Held open on purpose: the shim stops when this pipe closes.
    stdin: Option<ChildStdin>,
    /// Asks the task to kill the child. Dropping it does the same.
    stop: Option<oneshot::Sender<()>>,
    exit: watch::Receiver<Option<ShimExit>>,
}

impl ShimProcess {
    /// Start a VM host process for `spec`.
    ///
    /// `inherit_fd` is the guest end of the network socketpair. It is
    /// closed in the parent once the child has its own copy.
    ///
    /// # Errors
    ///
    /// Fails if the current executable cannot be started or the spec cannot
    /// be delivered.
    pub async fn spawn(spec: &VmSpec, inherit_fd: Option<OwnedFd>) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(exe);
        // libkrun writes the guest console to stdout. A host that speaks a
        // protocol on stdout (ward-mcp) would see those lines mixed into its
        // messages, so route the console to stderr.
        let console = std::io::stderr().as_fd().try_clone_to_owned()?;
        cmd.arg(SHIM_ARG)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::from(console))
            .kill_on_drop(true);
        if let Some(fd) = &inherit_fd {
            let raw = std::os::fd::AsRawFd::as_raw_fd(fd);
            // SAFETY: the closure runs between fork and exec and only calls
            // fcntl(2) on a descriptor that is open in the parent.
            unsafe {
                cmd.pre_exec(move || clear_close_on_exec(raw));
            }
        }
        let mut payload = serde_json::to_vec(spec).map_err(std::io::Error::other)?;
        payload.push(b'\n');
        let child = cmd.spawn()?;
        drop(inherit_fd);
        Self::watch(child, &payload).await
    }

    /// Start watching `child` and give it `payload` on its stdin.
    pub(super) async fn watch(mut child: Child, payload: &[u8]) -> std::io::Result<Self> {
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("VM host process ended at start"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("shim stdin was not piped"))?;
        let (stop, stop_rx) = oneshot::channel();
        let (exit_tx, exit) = watch::channel(None);
        tokio::spawn(supervise(
            child,
            stop_rx,
            exit_tx,
            std::time::Instant::now(),
        ));
        let mut shim = Self {
            pid,
            stdin: None,
            stop: Some(stop),
            exit,
        };
        // On an error `shim` drops here, which kills and reaps the child.
        stdin.write_all(payload).await?;
        stdin.flush().await?;
        shim.stdin = Some(stdin);
        Ok(shim)
    }

    /// Process id of the VM host process, for tests that signal it.
    #[cfg(test)]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// True while the child has not exited.
    pub fn is_running(&self) -> bool {
        self.exit.borrow().is_none()
    }

    /// Follows the child's exit. Holds `Some` once it has ended.
    pub fn exit_watch(&self) -> watch::Receiver<Option<ShimExit>> {
        self.exit.clone()
    }

    /// Stop the VM: close the control pipe, kill the child and reap it.
    pub async fn stop(&mut self) {
        self.stdin.take();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let ended = self.exit.wait_for(Option::is_some);
        if tokio::time::timeout(STOP_WAIT, ended).await.is_err() {
            tracing::warn!(pid = self.pid, "VM host process did not exit after kill");
        }
    }
}

/// Wait for `child` and publish how it ended. A stop request, or the handle
/// going away, kills it first.
async fn supervise(
    mut child: Child,
    stop: oneshot::Receiver<()>,
    exit: watch::Sender<Option<ShimExit>>,
    started: std::time::Instant,
) {
    let ended = tokio::select! {
        status = child.wait() => Some(status),
        _ = stop => None,
    };
    let status = match ended {
        Some(status) => status,
        None => {
            let _ = child.start_kill();
            child.wait().await
        }
    };
    let _ = exit.send(Some(ShimExit::of(status, started.elapsed())));
}

fn clear_close_on_exec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fd is open in the forked child, which is a copy of the parent.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty()).map_err(std::io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> VmSpec {
        VmSpec {
            rootfs: PathBuf::from("/data/sandboxes/abc/rootfs"),
            cpus: 2,
            memory_mb: 256,
            exec_path: "/ward-agent".to_string(),
            exec_args: vec![
                "--mount".into(),
                "ward-mnt-0".into(),
                "/data".into(),
                "ro".into(),
            ],
            agent_port: 1024,
            agent_socket: PathBuf::from("/tmp/ward-501/abc.sock"),
            mounts: vec![MountSpec {
                tag: "ward-mnt-0".into(),
                path: PathBuf::from("/host/dir"),
                read_only: true,
            }],
            net: Some(NetSpec {
                fd: 7,
                mac: [2, 0, 0, 0, 0, 2],
            }),
        }
    }

    #[test]
    fn given_more_cpus_than_libkrun_takes_when_converted_then_the_error_names_the_limit() {
        assert_eq!(vcpus(MAX_VCPUS), Ok(255));
        assert_eq!(
            vcpus(MAX_VCPUS + 1),
            Err("cpus=256 exceeds libkrun's limit of 255".to_string())
        );
    }

    #[test]
    fn given_spec_when_serialized_on_one_line_and_parsed_then_nothing_is_lost() {
        let json = serde_json::to_string(&sample_spec()).unwrap();

        assert!(!json.contains('\n'), "the shim reads the spec as one line");
        let parsed: VmSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.rootfs, PathBuf::from("/data/sandboxes/abc/rootfs"));
        assert_eq!((parsed.cpus, parsed.memory_mb), (2, 256));
        assert_eq!(parsed.exec_args.len(), 4);
        assert_eq!(parsed.mounts[0].tag, "ward-mnt-0");
        assert!(parsed.mounts[0].read_only);
        assert_eq!(parsed.net.unwrap().mac, [2, 0, 0, 0, 0, 2]);
    }

    #[test]
    fn given_argument_that_is_not_the_shim_flag_when_run_if_requested_then_it_returns() {
        // The test binary's own argv never carries SHIM_ARG, so this must
        // return instead of exiting the test process.
        run_if_requested();
    }

    const FAKE_SHIM_LIFETIME_SECS: &str = "60";
    const SIGKILL_NUMBER: i32 = 9;
    const EXIT_WAIT: Duration = Duration::from_secs(30);

    /// A long-lived child that stands in for the VM host process.
    async fn fake_shim() -> ShimProcess {
        let child = Command::new("sleep")
            .arg(FAKE_SHIM_LIFETIME_SECS)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        ShimProcess::watch(child, b"{}\n").await.expect("watch")
    }

    #[tokio::test]
    async fn given_shim_killed_from_outside_when_watched_then_the_exit_is_reported_at_once() {
        // Arrange
        let shim = fake_shim().await;
        let mut exit = shim.exit_watch();
        assert!(shim.is_running());

        // Act
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(shim.pid() as i32).expect("pid"),
            rustix::process::Signal::KILL,
        )
        .expect("kill");
        let ended = tokio::time::timeout(EXIT_WAIT, exit.wait_for(Option::is_some))
            .await
            .expect("exit seen")
            .expect("watch open")
            .expect("some");

        // Assert
        assert_eq!(ended.signal, Some(SIGKILL_NUMBER));
        assert_eq!(ended.code, None);
        assert!(!shim.is_running());
    }

    #[tokio::test]
    async fn given_running_shim_when_stopped_then_it_is_reaped_and_reported() {
        // Arrange
        let mut shim = fake_shim().await;

        // Act
        shim.stop().await;

        // Assert
        assert!(!shim.is_running());
        assert!(shim.exit_watch().borrow().is_some());
    }

    #[test]
    fn given_nul_byte_in_a_string_when_converted_to_cstring_then_error() {
        assert!(cstring("a\0b").is_err());
    }
}
