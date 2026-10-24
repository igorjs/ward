// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Pseudo-terminal support for tty execs: allocation, the async master end,
//! and wiring the slave to a child as its controlling terminal.

use std::io;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::pty::OpenptFlags;
use rustix::termios::{SpecialCodeIndex, Winsize};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::Command;

/// Async handle on the master side of a pty. Clones share one descriptor, so
/// the pty stays open until the last clone is dropped.
#[derive(Clone)]
pub struct PtyMaster(Arc<AsyncFd<OwnedFd>>);

/// Allocate a pty pair. The slave is blocking and close-on-exec.
pub fn open() -> io::Result<(PtyMaster, OwnedFd)> {
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let slave_path = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        slave_path.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    let flags = rustix::fs::fcntl_getfl(&master)?;
    rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK)?;
    // SAFETY: `master` is an open descriptor owned by the `AsyncFd` from here
    // on, and nothing else closes or replaces it.
    let registered = unsafe { AsyncFd::register(master) }.map_err(|e| e.into_parts().1)?;
    Ok((PtyMaster(Arc::new(registered)), slave))
}

/// Make `slave` the child's stdin, stdout, stderr and controlling terminal,
/// in a new session. The caller must drop `cmd` after spawning so the parent
/// releases its copies of the slave.
pub fn attach_slave(cmd: &mut Command, slave: OwnedFd) -> io::Result<()> {
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: the closure runs between fork and exec and calls only
    // setsid(2) and ioctl(2), both async-signal-safe, with no allocation.
    unsafe {
        cmd.pre_exec(move || {
            rustix::process::setsid().map_err(io::Error::from)?;
            rustix::process::ioctl_tiocsctty(&slave).map_err(io::Error::from)
        });
    }
    Ok(())
}

impl PtyMaster {
    /// Set the terminal size; the foreground process group receives SIGWINCH.
    pub fn set_window_size(&self, rows: u32, cols: u32) -> io::Result<()> {
        let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
        let size = Winsize {
            ws_row: clamp(rows),
            ws_col: clamp(cols),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        rustix::termios::tcsetwinsize(self.0.get_ref(), size).map_err(io::Error::from)
    }

    /// Give the child end-of-file by writing the terminal's EOF character,
    /// since a pty has no half-close.
    pub async fn send_eof(&self) -> io::Result<()> {
        let termios = rustix::termios::tcgetattr(self.0.get_ref())?;
        let eof = termios.special_codes[SpecialCodeIndex::VEOF];
        self.clone().write_all(&[eof]).await
    }
}

impl AsyncRead for PtyMaster {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            match rustix::io::read(self.0.get_ref(), buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(Errno::AGAIN) => guard.clear_ready(),
                Err(Errno::INTR) => {}
                // Every slave handle closed: the terminal hung up, so EOF.
                Err(Errno::IO) => return Poll::Ready(Ok(())),
                Err(e) => return Poll::Ready(Err(e.into())),
            }
        }
    }
}

impl AsyncWrite for PtyMaster {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match rustix::io::write(self.0.get_ref(), buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(Errno::AGAIN) => guard.clear_ready(),
                Err(Errno::INTR) => {}
                Err(e) => return Poll::Ready(Err(e.into())),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// A pty has no half-close, so this is a no-op; the master closes when
    /// the last clone drops.
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
