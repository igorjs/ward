// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Device-layer tests for `RawFdDevice`.
//!
//! Uses a raw `socketpair(2)` pair so a test can write bytes on one end
//! and assert `RawFdDevice::receive` observes them on the other, with no
//! smoltcp `Interface` involved.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use smoltcp::phy::{Device, RxToken, TxToken};
use smoltcp::time::Instant;
use ward_net::smoltcp_backend::RawFdDevice;

/// Create an `AF_UNIX SOCK_DGRAM` pair and return both ends as owned fds.
///
/// Uses `SOCK_DGRAM` so each `write` produces one discrete datagram
/// that a single `receive` call can observe.
fn socketpair_dgram() -> (OwnedFd, OwnedFd) {
    let mut sv: [std::ffi::c_int; 2] = [-1, -1];
    // SAFETY: socketpair is a pure syscall with no preconditions beyond a
    // valid `sv` pointer; both fds are closed on drop via OwnedFd.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, sv.as_mut_ptr()) };
    assert_eq!(
        ret,
        0,
        "socketpair(AF_UNIX, SOCK_DGRAM) failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: socketpair succeeded; sv[0] and sv[1] are valid open fds.
    let a = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let b = unsafe { OwnedFd::from_raw_fd(sv[1]) };
    (a, b)
}

/// Write `frame` as a single datagram on `fd`.
fn write_frame(fd: &OwnedFd, frame: &[u8]) {
    // SAFETY: fd is a valid open socket for the duration of this call;
    // frame's pointer and length describe a valid, initialized slice.
    let ret = unsafe { libc::write(fd.as_raw_fd(), frame.as_ptr().cast(), frame.len()) };
    assert_eq!(
        ret,
        frame.len() as isize,
        "write to socketpair should write the whole frame in one datagram: {}",
        std::io::Error::last_os_error()
    );
}

/// Read one datagram off `fd`, blocking until it arrives.
fn read_frame(fd: &OwnedFd) -> Vec<u8> {
    let mut buf = [0u8; 1514];
    // SAFETY: fd is a valid open socket for the duration of this call;
    // buf is a valid, initialized buffer of the given length.
    let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    assert!(
        n > 0,
        "recv from socketpair should return the transmitted datagram: {}",
        std::io::Error::last_os_error()
    );
    buf[..n as usize].to_vec()
}

#[test]
fn given_frame_on_fd_when_receive_then_device_returns_it() {
    // Arrange
    let (write_end, read_end) = socketpair_dgram();
    let frame: [u8; 14] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // dst mac (broadcast)
        0x02, 0x00, 0x00, 0x00, 0x00, 0x01, // src mac
        0x08, 0x00, // ethertype: IPv4
    ];
    write_frame(&write_end, &frame);
    let mut device = RawFdDevice::new(read_end);

    // Act
    let (rx_token, _tx_token) = device
        .receive(Instant::now())
        .expect("receive should return a token pair for the pending datagram");
    let received: Vec<u8> = rx_token.consume(|buf| buf.to_vec());

    // Assert
    assert_eq!(received, frame.to_vec());
}

#[test]
fn given_no_data_when_receive_then_returns_none_without_blocking() {
    // Arrange
    let (_write_end, read_end) = socketpair_dgram();
    let mut device = RawFdDevice::new(read_end);

    // Act
    let started = std::time::Instant::now();
    let result = device.receive(Instant::now());
    let elapsed = started.elapsed();

    // Assert
    assert!(
        result.is_none(),
        "receive on an empty socket should return None, not a token pair"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(100),
        "receive on an empty socket should return immediately (MSG_DONTWAIT), \
         took {elapsed:?} instead, which suggests it blocked"
    );
}

#[test]
fn given_truncated_frame_on_fd_when_receive_then_device_drops_it_without_panic() {
    // Arrange: an Ethernet frame needs at least 14 bytes (6 dst mac + 6 src
    // mac + 2 ethertype); this datagram is shorter than that minimum.
    let (write_end, read_end) = socketpair_dgram();
    let truncated: [u8; 5] = [0xff, 0xff, 0xff, 0xff, 0xff];
    write_frame(&write_end, &truncated);
    let mut device = RawFdDevice::new(read_end);

    // Act
    let result = device.receive(Instant::now());

    // Assert: the too-short datagram is dropped, not handed to smoltcp's
    // Interface parser as if it were a valid frame.
    assert!(
        result.is_none(),
        "receive on a truncated (sub-minimum-length) datagram should return \
         None, not a token pair wrapping the truncated bytes"
    );
}

#[test]
fn given_oversized_frame_on_fd_when_receive_then_device_drops_it_without_panic() {
    // Arrange: AF_UNIX SOCK_DGRAM sockets can carry a datagram well past
    // 1514 bytes (there is no Ethernet-imposed limit at the socket layer),
    // so an oversized datagram is a genuine input the device must reject.
    let (write_end, read_end) = socketpair_dgram();
    let oversized: [u8; 1600] = [0xaa; 1600];
    write_frame(&write_end, &oversized);
    let mut device = RawFdDevice::new(read_end);

    // Act
    let result = device.receive(Instant::now());

    // Assert: the oversized datagram is dropped, not handed back as a
    // truncated frame or passed on to smoltcp's Interface parser.
    assert!(
        result.is_none(),
        "receive on a datagram larger than the max Ethernet frame length \
         should return None, not a token pair wrapping truncated bytes"
    );
}

#[test]
fn given_device_when_transmit_then_frame_appears_on_fd() {
    // Arrange
    let (device_end, observer_end) = socketpair_dgram();
    let frame: [u8; 14] = [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x01, // dst mac
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // src mac (broadcast, reversed vs the rx test)
        0x08, 0x06, // ethertype: ARP
    ];
    let mut device = RawFdDevice::new(device_end);

    // Act
    let tx_token = device
        .transmit(Instant::now())
        .expect("transmit should return a token to write a frame");
    tx_token.consume(frame.len(), |buf| {
        buf.copy_from_slice(&frame);
    });
    let observed = read_frame(&observer_end);

    // Assert
    assert_eq!(observed, frame.to_vec());
}
