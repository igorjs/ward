// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Terminal raw mode with guaranteed restore.
//!
//! [`RawModeGuard`] restores the saved settings on drop, which also covers a
//! panic that unwinds. [`ExitSignals`] turns SIGINT, SIGTERM and SIGHUP into
//! an exit code the caller returns, so the guard still drops.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::termios::{self, OptionalActions, Termios};
use tokio::signal::unix::{Signal, SignalKind, signal};

/// Shell convention: a process ended by signal N exits with 128 + N.
const SIGNAL_EXIT_BASE: i32 = 128;

/// Terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

/// Returns the size of the terminal behind `fd`.
pub fn window_size(fd: impl AsFd) -> io::Result<WindowSize> {
    let size = termios::tcgetwinsize(fd)?;
    Ok(WindowSize {
        rows: size.ws_row,
        cols: size.ws_col,
    })
}

/// Terminal settings to put back, with the descriptor they belong to.
struct Saved {
    fd: OwnedFd,
    termios: Termios,
}

impl Saved {
    fn restore(&self) {
        // Nothing useful can be done if the terminal is already gone.
        let _ = termios::tcsetattr(&self.fd, OptionalActions::Flush, &self.termios);
    }
}

/// Holds a terminal in raw mode until dropped.
pub struct RawModeGuard {
    saved: Saved,
}

impl RawModeGuard {
    /// Switches the terminal behind `fd` to raw mode.
    ///
    /// Fails when `fd` is not a terminal.
    pub fn enable(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let original = termios::tcgetattr(fd)?;
        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(fd, OptionalActions::Flush, &raw)?;
        Ok(Self {
            saved: Saved {
                fd: fd.try_clone_to_owned()?,
                termios: original,
            },
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.saved.restore();
    }
}

/// Signals that end the CLI. Waiting on them lets the caller unwind, so
/// destructors and cleanup run before the process exits.
pub struct ExitSignals {
    interrupt: Option<Signal>,
    terminate: Option<Signal>,
    hangup: Signal,
}

impl ExitSignals {
    /// Registers the handlers now. SIGHUP always ends the CLI; SIGINT and
    /// SIGTERM only when `include_interrupts` is set, so the caller can
    /// forward those elsewhere instead. Must run inside a tokio runtime.
    pub fn register(include_interrupts: bool) -> io::Result<Self> {
        let optional = |kind| include_interrupts.then(|| signal(kind)).transpose();
        Ok(Self {
            interrupt: optional(SignalKind::interrupt())?,
            terminate: optional(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolves with the exit code (`128 + signo`) of the first signal.
    pub async fn recv_exit_code(&mut self) -> i32 {
        let signo = tokio::select! {
            () = recv_or_pend(&mut self.interrupt) => SignalKind::interrupt(),
            () = recv_or_pend(&mut self.terminate) => SignalKind::terminate(),
            _ = self.hangup.recv() => SignalKind::hangup(),
        };
        SIGNAL_EXIT_BASE + signo.as_raw_value()
    }
}

async fn recv_or_pend(stream: &mut Option<Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsFd;
    use std::process::{Command, Stdio};

    use rustix::fs::{Mode, OFlags, open};
    use rustix::process::{Pid, Signal as RustixSignal, kill_process};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    const MODE_ENV: &str = "WARD_TERMINAL_TEST_MODE";
    const CHILD_TEST: &str = "terminal::tests::child_entry";
    const READY_LINE: &str = "ready";
    const IDLE_SECONDS: u64 = 30;
    const PANIC_EXIT_CODE: i32 = 101;
    const TEST_ROWS: u16 = 40;
    const TEST_COLS: u16 = 80;

    /// Child side: enables raw mode, reports readiness, then follows `MODE_ENV`.
    /// Does nothing when run as a normal test.
    #[test]
    fn child_entry() {
        let Ok(mode) = std::env::var(MODE_ENV) else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let stdin = std::io::stdin();
            let guard = RawModeGuard::enable(stdin.as_fd()).expect("raw mode");
            let mut exits = ExitSignals::register(true).expect("handlers");
            println!("{READY_LINE}");
            match mode.as_str() {
                "exit" => {}
                "panic" => panic!("wrapped child failed"),
                _ => {
                    let code = tokio::select! {
                        code = exits.recv_exit_code() => code,
                        () = tokio::time::sleep(std::time::Duration::from_secs(IDLE_SECONDS)) => 0,
                    };
                    drop(guard);
                    std::process::exit(code);
                }
            }
        });
    }

    fn exit_code_for(signal: RustixSignal) -> i32 {
        SIGNAL_EXIT_BASE + signal.as_raw()
    }

    fn open_pty() -> (OwnedFd, File) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let name = ptsname(&master, Vec::new()).expect("ptsname");
        let slave = open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .expect("open slave");
        (master, File::from(slave))
    }

    fn flags(t: &Termios) -> String {
        format!(
            "{:?} {:?} {:?} {:?}",
            t.input_modes, t.output_modes, t.control_modes, t.local_modes
        )
    }

    #[test]
    fn window_size_reads_what_was_set() {
        // Arrange
        let (_master, slave) = open_pty();
        let size = termios::Winsize {
            ws_row: TEST_ROWS,
            ws_col: TEST_COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        termios::tcsetwinsize(&slave, size).expect("set size");

        // Act
        let read = window_size(&slave).expect("window size");

        // Assert
        assert_eq!(
            read,
            WindowSize {
                rows: TEST_ROWS,
                cols: TEST_COLS
            }
        );
    }

    #[test]
    fn enable_fails_when_fd_is_not_a_terminal() {
        // Arrange
        let file = File::open("/dev/null").expect("open");

        // Act
        let result = RawModeGuard::enable(file.as_fd());

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn terminal_is_restored_after_child_ends() {
        let cases: [(&str, Option<RustixSignal>, i32); 5] = [
            ("exit", None, 0),
            (
                "sigint",
                Some(RustixSignal::INT),
                exit_code_for(RustixSignal::INT),
            ),
            (
                "sigterm",
                Some(RustixSignal::TERM),
                exit_code_for(RustixSignal::TERM),
            ),
            (
                "sighup",
                Some(RustixSignal::HUP),
                exit_code_for(RustixSignal::HUP),
            ),
            ("panic", None, PANIC_EXIT_CODE),
        ];
        for (mode, sent, expected_code) in cases {
            // Arrange
            let (_master, slave) = open_pty();
            let before = termios::tcgetattr(&slave).expect("tcgetattr");
            let mut child = Command::new(std::env::current_exe().expect("exe"))
                .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(MODE_ENV, mode)
                .stdin(Stdio::from(slave.try_clone().expect("dup")))
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child");
            let stdout = child.stdout.take().expect("stdout");
            let mut lines = BufReader::new(stdout).lines();
            // libtest prefixes the line with the test name.
            while !lines
                .next()
                .expect("child output")
                .expect("line")
                .ends_with(READY_LINE)
            {}

            // Act
            if let Some(signal) = sent {
                let pid = Pid::from_raw(child.id() as i32).expect("pid");
                kill_process(pid, signal).expect("kill");
            }
            let status = child.wait().expect("wait");

            // Assert
            let after = termios::tcgetattr(&slave).expect("tcgetattr");
            assert_eq!(status.code(), Some(expected_code), "exit code for {mode}");
            assert_eq!(flags(&after), flags(&before), "termios restored for {mode}");
        }
    }
}
