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
use std::os::fd::{OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

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

        if spec.cpus > 0 {
            let vcpus = u8::try_from(spec.cpus)
                .map_err(|_| format!("cpus={} exceeds libkrun's limit of 255", spec.cpus))?;
            check(
                krun_ffi::krun_set_vm_config(ctx, vcpus, spec.memory_mb),
                "krun_set_vm_config",
            )?;
        }
        check(krun_ffi::krun_set_root(ctx, root.as_ptr()), "krun_set_root")?;

        if let Some(net) = &spec.net {
            krun_ffi::set_net_unixgram(ctx, net.fd, net.mac)?;
        }
        for mount in &spec.mounts {
            let tag = cstring(&mount.tag)?;
            let path = path_cstring(&mount.path)?;
            check(
                krun_ffi::krun_add_virtiofs3(ctx, tag.as_ptr(), path.as_ptr(), 0, mount.read_only),
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

/// Handle to a running VM host process.
#[derive(Debug)]
pub struct ShimProcess {
    child: Child,
    /// Held open on purpose: the shim stops when this pipe closes.
    stdin: Option<ChildStdin>,
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
        cmd.arg(SHIM_ARG)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(fd) = &inherit_fd {
            let raw = std::os::fd::AsRawFd::as_raw_fd(fd);
            // SAFETY: the closure runs between fork and exec and only calls
            // fcntl(2) on a descriptor that is open in the parent.
            unsafe {
                cmd.pre_exec(move || clear_close_on_exec(raw));
            }
        }
        let mut child = cmd.spawn()?;
        drop(inherit_fd);

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("shim stdin was not piped"))?;
        let mut payload = serde_json::to_vec(spec).map_err(std::io::Error::other)?;
        payload.push(b'\n');
        stdin.write_all(&payload).await?;
        stdin.flush().await?;
        Ok(Self {
            child,
            stdin: Some(stdin),
        })
    }

    /// True while the child has not exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop the VM: close the control pipe, kill the child and reap it.
    pub async fn stop(&mut self) {
        self.stdin.take();
        let _ = self.child.start_kill();
        if tokio::time::timeout(STOP_WAIT, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!("VM host process did not exit after kill");
        }
    }
}

fn clear_close_on_exec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fd is open in the forked child, which is a copy of the parent.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty()).map_err(std::io::Error::from)
}
