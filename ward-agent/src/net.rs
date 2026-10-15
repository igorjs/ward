// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Guest network setup.
//!
//! The host side (`ward-net`) answers on `10.0.2.2` and hands out leases from
//! `10.0.2.15`. Rootfs images cannot be assumed to ship `ip` or a DHCP client,
//! so the agent configures `eth0` directly with ioctls: a static address, a
//! default route through the gateway and a resolver pointing at the gateway.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Interface the VMM attaches the virtio-net device to.
pub const INTERFACE: &str = "eth0";
/// Address the guest takes. First address of the host stack's DHCP pool.
pub const GUEST_ADDR: [u8; 4] = [10, 0, 2, 15];
pub const NETMASK: [u8; 4] = [255, 255, 255, 0];
/// The host stack: default gateway and DNS relay.
pub const GATEWAY_ADDR: [u8; 4] = [10, 0, 2, 2];
const RESOLV_CONF_DIR: &str = "/etc";
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

/// True when the VMM attached a network device (egress is not `deny`).
#[must_use]
pub fn interface_present() -> bool {
    std::path::Path::new("/sys/class/net")
        .join(INTERFACE)
        .exists()
}

/// Bring the interface up with the static configuration above.
///
/// # Errors
///
/// Fails if any ioctl or the resolver file write fails.
pub fn configure() -> io::Result<()> {
    let sock = datagram_socket()?;
    set_addr(&sock, libc::SIOCSIFADDR, GUEST_ADDR)?;
    set_addr(&sock, libc::SIOCSIFNETMASK, NETMASK)?;
    set_link_up(&sock)?;
    add_default_route(&sock)?;
    std::fs::create_dir_all(RESOLV_CONF_DIR)?;
    let [a, b, c, d] = GATEWAY_ADDR;
    std::fs::write(RESOLV_CONF_PATH, format!("nameserver {a}.{b}.{c}.{d}\n"))
}

fn datagram_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket(2) call; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a freshly created descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn sockaddr_in(octets: [u8; 4]) -> libc::sockaddr_in {
    // SAFETY: sockaddr_in is plain data; all-zero is a valid value.
    let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(octets),
    };
    addr
}

fn ifreq() -> libc::ifreq {
    // SAFETY: ifreq is plain data; all-zero is a valid value.
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(INTERFACE.bytes()) {
        *slot = byte as libc::c_char;
    }
    req
}

fn ioctl<T>(sock: &OwnedFd, request: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` points to a live struct of the type `request` expects.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), request as _, std::ptr::from_mut(arg)) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_addr(sock: &OwnedFd, request: libc::c_ulong, octets: [u8; 4]) -> io::Result<()> {
    let mut req = ifreq();
    let addr = sockaddr_in(octets);
    // SAFETY: sockaddr_in and sockaddr have the same size; the union field
    // is written whole before the ioctl reads it.
    unsafe {
        std::ptr::write(
            std::ptr::addr_of_mut!(req.ifr_ifru.ifru_addr).cast::<libc::sockaddr_in>(),
            addr,
        );
    }
    ioctl(sock, request, &mut req)
}

fn set_link_up(sock: &OwnedFd) -> io::Result<()> {
    let mut req = ifreq();
    ioctl(sock, libc::SIOCGIFFLAGS, &mut req)?;
    // SAFETY: SIOCGIFFLAGS filled the flags member of the union.
    unsafe {
        req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    }
    ioctl(sock, libc::SIOCSIFFLAGS, &mut req)
}

fn add_default_route(sock: &OwnedFd) -> io::Result<()> {
    // SAFETY: rtentry is plain data; all-zero is a valid value.
    let mut route: libc::rtentry = unsafe { mem::zeroed() };
    let as_generic = |octets| {
        let addr = sockaddr_in(octets);
        // SAFETY: both types are plain data of the same size.
        unsafe { mem::transmute::<libc::sockaddr_in, libc::sockaddr>(addr) }
    };
    route.rt_dst = as_generic([0; 4]);
    route.rt_genmask = as_generic([0; 4]);
    route.rt_gateway = as_generic(GATEWAY_ADDR);
    route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    ioctl(sock, libc::SIOCADDRT, &mut route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_octets_when_building_sockaddr_then_family_and_address_are_set() {
        let addr = sockaddr_in([10, 0, 2, 15]);
        assert_eq!(i32::from(addr.sin_family), libc::AF_INET);
        assert_eq!(addr.sin_addr.s_addr.to_ne_bytes(), [10, 0, 2, 15]);
    }

    #[test]
    fn given_interface_name_when_building_ifreq_then_name_is_nul_terminated() {
        let req = ifreq();
        assert_eq!(req.ifr_name[0] as u8, b'e');
        assert_eq!(req.ifr_name[INTERFACE.len()], 0);
    }
}
