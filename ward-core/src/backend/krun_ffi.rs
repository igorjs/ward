// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Runtime loader and FFI declarations for [libkrun](https://github.com/containers/libkrun).
//!
//! libkrun and libkrunfw are embedded in the binary by `ward-bundle`. The
//! first call unpacks them and opens them with `dlopen`, so nothing is
//! linked at build time and nothing has to be installed on the host.
//!
//! Only the symbols ward calls are declared. To use another one, add its
//! signature from `include/libkrun.h` to the `krun_api!` list below. A symbol
//! that the loaded library lacks makes its wrapper return `-ENOSYS` instead of
//! failing the whole load.
//!
//! # Safety
//!
//! Every wrapper is an `unsafe fn` over a libkrun function. Callers must pass
//! a `ctx_id` returned by `krun_create_ctx` that has not been freed, valid
//! NUL-terminated C strings, and null-terminated arrays where the header
//! asks for them. libkrun returns a negative errno on failure.

use std::ffi::c_char;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::OnceLock;

use libloading::Library;

/// `-ENOSYS`: returned by wrappers whose symbol the loaded libkrun lacks.
const NEGATIVE_ENOSYS: i32 = -38;

/// Generates, for each libkrun function, an optional resolved pointer on
/// [`Api`] and a same-named `unsafe fn` wrapper that dispatches to it.
macro_rules! krun_api {
    ($( pub fn $name:ident($($arg:ident : $ty:ty),* $(,)?) -> i32; )*) => {
        struct Api {
            $( $name: Option<unsafe extern "C" fn($($ty),*) -> i32>, )*
            _krun: Library,
            _krunfw: Library,
        }

        impl Api {
            fn resolve(krun: Library, krunfw: Library) -> Self {
                $(
                    // SAFETY: the declared signature mirrors libkrun.h for this symbol.
                    let $name = unsafe {
                        krun.get::<unsafe extern "C" fn($($ty),*) -> i32>(
                            concat!(stringify!($name), "\0").as_bytes(),
                        )
                    }
                    .ok()
                    .map(|sym| *sym);
                )*
                Self { $( $name, )* _krun: krun, _krunfw: krunfw }
            }
        }

        $(
            pub unsafe fn $name($($arg: $ty),*) -> i32 {
                match api().and_then(|a| a.$name) {
                    // SAFETY: forwarded under the caller's contract for this libkrun function.
                    Some(f) => unsafe { f($($arg),*) },
                    None => NEGATIVE_ENOSYS,
                }
            }
        )*
    };
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(load).as_ref().ok()
}

/// Load libkrun now. Safe to call repeatedly.
pub fn ensure_loaded() -> Result<(), &'static str> {
    match API.get_or_init(load) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.as_str()),
    }
}

fn load() -> Result<Api, String> {
    let base: PathBuf = ward_bundle::default_runtime_dir();
    let paths = ward_bundle::extract_libs(&base)
        .map_err(|e| format!("unpack embedded libkrun into {}: {e}", base.display()))?;
    // libkrun dlopens libkrunfw (its kernel) by file name when a VM boots.
    // Loading it first by absolute path makes that lookup resolve to this copy.
    // SAFETY: loading a library runs its initialisers; these are the pinned,
    // checksum-verified libkrun-builds artefacts.
    let krunfw = unsafe { Library::new(&paths.krunfw) }
        .map_err(|e| format!("load {}: {e}", paths.krunfw.display()))?;
    // SAFETY: as above.
    let krun = unsafe { Library::new(&paths.krun) }
        .map_err(|e| format!("load {}: {e}", paths.krun.display()))?;
    Ok(Api::resolve(krun, krunfw))
}

krun_api! {
    pub fn krun_set_log_level(level: u32) -> i32;
    pub fn krun_create_ctx() -> i32;
    pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;
    pub fn krun_add_virtiofs3(
        ctx_id: u32,
        c_tag: *const c_char,
        c_path: *const c_char,
        shm_size: u64,
        read_only: bool,
    ) -> i32;
    pub fn krun_add_net_unixgram(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *mut u8,
        features: u32,
        flags: u32,
    ) -> i32;
    pub fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;
    pub fn krun_disable_implicit_vsock(ctx_id: u32) -> i32;
    pub fn krun_add_vsock(ctx_id: u32, tsi_features: u32) -> i32;
    pub fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        c_filepath: *const c_char,
        listen: bool,
    ) -> i32;
    // Blocks until the guest ends: call it on a dedicated process or thread.
    pub fn krun_start_enter(ctx_id: u32) -> i32;
}

/// Hand the guest end of a datagram socketpair to libkrun as the VM's
/// virtio-net device. libkrun duplicates `fd`, so the caller may close its copy.
///
/// # Errors
///
/// Returns the libkrun errno as text when the call fails.
pub fn set_net_unixgram(ctx_id: u32, fd: RawFd, mac: [u8; 6]) -> Result<(), String> {
    let mut mac_bytes = mac;
    // SAFETY: `ctx_id` comes from `krun_create_ctx`, `fd` is open for this
    // call, the path is null because this is the fd-based mode, and
    // `mac_bytes` is a live 6-byte array.
    let ret = unsafe {
        krun_add_net_unixgram(ctx_id, std::ptr::null(), fd, mac_bytes.as_mut_ptr(), 0, 0)
    };
    if ret < 0 {
        Err(format!("krun_add_net_unixgram failed: errno {}", -ret))
    } else {
        Ok(())
    }
}
