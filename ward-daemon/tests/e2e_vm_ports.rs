// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! UDP ports and port ranges in a real guest. Needs `--features krunvm` and
//! `WARD_E2E_VM=1`; `WARD_E2E_IMAGE` overrides the image.
//!
//!   WARD_E2E_VM=1 cargo test -p ward-daemon --features krunvm --test e2e_vm_ports

#![cfg(feature = "krunvm")]

mod common;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use common::Daemon;

const ENABLE_ENV: &str = "WARD_E2E_VM";
const IMAGE_ENV: &str = "WARD_E2E_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const GUEST_REPLY: &str = "ward-guest-reply";
const GUEST_UDP_PORT: u16 = 5300;
const GUEST_TCP_PORT: u16 = 80;
const RANGE_SPAN: u16 = 9;
/// Highest start for the host range, so its end is a valid port.
const RANGE_START_CEILING: u16 = 65_000;
const DIAL_DEADLINE: Duration = Duration::from_secs(30);
const RETRY_STEP: Duration = Duration::from_millis(200);
const READ_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Create a sandbox with `flags`, run `script` in it and return its id.
fn serve(d: &Daemon, flags: &[&str], script: &str) -> String {
    let image = image();
    let mut args = vec!["create"];
    args.extend(flags);
    args.push(image.as_str());
    let id = run_ok(d, &args).trim().to_string();
    run_ok(d, &["exec", "-d", &id, "--", "sh", "-c", script]);
    id
}

/// Send a datagram until the guest answers, then return the answer.
fn udp_reply(port: u16) -> String {
    let deadline = Instant::now() + DIAL_DEADLINE;
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
    socket
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("timeout");
    loop {
        let mut buf = [0u8; 256];
        let got = socket
            .send_to(b"ping", ("127.0.0.1", port))
            .and_then(|_| socket.recv_from(&mut buf));
        if let Ok((len, _)) = got {
            return String::from_utf8_lossy(&buf[..len]).into_owned();
        }
        assert!(Instant::now() < deadline, "no UDP reply on port {port}");
        std::thread::sleep(RETRY_STEP);
    }
}

/// A port the OS just handed out, low enough to start a range from.
fn free_port() -> u16 {
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        if port <= RANGE_START_CEILING {
            return port;
        }
    }
}

fn tcp_reply(port: u16) -> String {
    use std::io::Read;
    let deadline = Instant::now() + DIAL_DEADLINE;
    loop {
        let mut reply = String::new();
        let read = std::net::TcpStream::connect(("127.0.0.1", port))
            .and_then(|mut stream| stream.read_to_string(&mut reply));
        if read.is_ok() && !reply.is_empty() {
            return reply;
        }
        assert!(Instant::now() < deadline, "no TCP reply on port {port}");
        std::thread::sleep(RETRY_STEP);
    }
}

#[test]
fn given_a_udp_port_when_the_host_sends_a_datagram_then_the_guest_replies() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let publish = format!("0:{GUEST_UDP_PORT}/udp");
    let script =
        format!("while true; do printf '{GUEST_REPLY}' | nc -u -l -p {GUEST_UDP_PORT}; done");
    let id = serve(&d, &["-p", &publish], &script);
    let listed = run_ok(&d, &["port", &id, &format!("{GUEST_UDP_PORT}/udp")]);
    let host_port: u16 = listed.trim().rsplit(':').next().unwrap().parse().unwrap();

    // Act
    let reply = udp_reply(host_port);

    // Assert
    assert_eq!(reply, GUEST_REPLY);
    assert!(run_ok(&d, &["ps", "--format", "{{.Ports}}"]).contains("/udp"));
}

#[test]
fn given_a_host_port_range_when_published_then_a_port_in_the_range_serves_the_guest() {
    require_vm!();
    // Arrange
    let d = Daemon::spawn_vm();
    let range_first = free_port();
    let range_last = range_first + RANGE_SPAN;
    let publish = format!("{range_first}-{range_last}:{GUEST_TCP_PORT}");
    let script = format!("while true; do printf '{GUEST_REPLY}' | nc -l -p {GUEST_TCP_PORT}; done");
    let id = serve(&d, &["-p", &publish], &script);

    // Act
    let listed = run_ok(&d, &["port", &id, &GUEST_TCP_PORT.to_string()]);
    let host_port: u16 = listed.trim().rsplit(':').next().unwrap().parse().unwrap();

    // Assert
    assert!((range_first..=range_last).contains(&host_port), "{listed}");
    assert_eq!(tcp_reply(host_port), GUEST_REPLY);
}
