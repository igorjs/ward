// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Hand-maintained FFI declarations for [libkrun](https://github.com/containers/libkrun).
//!
//! # Why hand-maintained
//!
//! The upstream `krun-sys` crate on crates.io is a thin wrapper over
//! `bindgen` + `pkg-config` — 33 lines of glue plus auto-generated
//! bindings. For an API as stable and simple as libkrun's (60
//! functions, all primitive/string/array signatures, no nested structs,
//! no callback types), bindgen's value-add is minimal and its
//! build-time dependency on `libclang` adds real friction (broke
//! `cargo build --features krunvm` previously).
//!
//! Declaring each function by hand here:
//!
//! - Removes the `bindgen` + `clang-sys` + `pkg-config` build-deps
//!   (~150 transitive crates).
//! - Eliminates the `libclang` system requirement at build time.
//! - Exposes **all 60** libkrun 1.18.0 symbols, not the 30 that
//!   crates.io's krun-sys 1.10.1 shipped against.
//! - Trivially auditable — `diff` this file against
//!   `containers/libkrun/include/libkrun.h` to verify correctness.
//!
//! # Maintenance
//!
//! When libkrun upstream bumps, run:
//!
//! ```bash
//! curl -sL https://raw.githubusercontent.com/containers/libkrun/v<NEW>/include/libkrun.h \
//!   | grep -E '^int32_t krun_|^uint32_t krun_|^void krun_'
//! ```
//!
//! and translate any new signatures into the `unsafe extern "C"`
//! block below. The same convention is documented in CONTRIBUTING.md.
//!
//! # Safety
//!
//! All functions here are `unsafe extern "C"`. Every call site in
//! `ward-core` must justify the safety of its invocation (typically:
//! `ctx_id` came from `krun_create_ctx()` and hasn't been freed; C
//! strings are valid NUL-terminated UTF-8; arrays end with a null
//! pointer sentinel where required).
//!
//! libkrun returns negative `int32_t` on failure (errno-style). Call
//! sites translate to `BackendError::Internal` with `-ret` as the
//! errno.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)] // many symbols are unused today; declared for completeness

use std::ffi::{c_char, c_int, c_void};

// `uid_t` and `gid_t` are platform-typed but in practice always u32 on
// every POSIX system ward supports (Linux + macOS arm64). Declaring as
// `u32` avoids pulling in the `libc` crate just for two type aliases.
type uid_t = u32;
type gid_t = u32;

unsafe extern "C" {
    // ---- Context lifecycle --------------------------------------------------

    pub fn krun_create_ctx() -> i32;
    pub fn krun_free_ctx(ctx_id: u32) -> i32;

    // ---- VM configuration ---------------------------------------------------

    pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;
    pub fn krun_set_root_disk(ctx_id: u32, disk_path: *const c_char) -> i32;
    pub fn krun_set_data_disk(ctx_id: u32, disk_path: *const c_char) -> i32;
    pub fn krun_add_disk(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        read_only: bool,
    ) -> i32;
    pub fn krun_add_disk2(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        disk_format: u32,
        read_only: bool,
    ) -> i32;
    pub fn krun_set_mapped_volumes(ctx_id: u32, mapped_volumes: *const *const c_char) -> i32;
    pub fn krun_add_virtiofs(ctx_id: u32, c_tag: *const c_char, c_path: *const c_char) -> i32;
    pub fn krun_add_virtiofs2(
        ctx_id: u32,
        c_tag: *const c_char,
        c_path: *const c_char,
        shm_size: u64,
    ) -> i32;
    pub fn krun_add_virtiofs3(
        ctx_id: u32,
        c_tag: *const c_char,
        c_path: *const c_char,
        shm_size: u64,
        read_only: bool,
    ) -> i32;

    // ---- Networking ---------------------------------------------------------

    pub fn krun_add_net_unixstream(
        ctx_id: u32,
        c_path: *const c_char,
        fd: c_int,
        c_mac: *mut u8,
        features: u32,
        flags: u32,
    ) -> i32;
    pub fn krun_add_net_unixgram(
        ctx_id: u32,
        c_path: *const c_char,
        fd: c_int,
        c_mac: *mut u8,
        features: u32,
        flags: u32,
    ) -> i32;
    pub fn krun_add_net_tap(
        ctx_id: u32,
        c_tap_name: *mut c_char,
        c_mac: *mut u8,
        features: u32,
        flags: u32,
    ) -> i32;
    pub fn krun_set_passt_fd(ctx_id: u32, fd: c_int) -> i32;
    pub fn krun_set_gvproxy_path(ctx_id: u32, c_path: *mut c_char) -> i32;
    pub fn krun_set_net_mac(ctx_id: u32, c_mac: *mut u8) -> i32;
    pub fn krun_set_port_map(ctx_id: u32, port_map: *const *const c_char) -> i32;

    // ---- GPU + display ------------------------------------------------------

    pub fn krun_set_gpu_options(ctx_id: u32, virgl_flags: u32) -> i32;
    pub fn krun_set_gpu_options2(ctx_id: u32, virgl_flags: u32, shm_size: u64) -> i32;
    pub fn krun_add_display(ctx_id: u32, width: u32, height: u32) -> i32;
    pub fn krun_display_set_edid(
        ctx_id: u32,
        display_id: u32,
        edid_blob: *const u8,
        blob_size: usize,
    ) -> i32;
    pub fn krun_display_set_dpi(ctx_id: u32, display_id: u32, dpi: u32) -> i32;
    pub fn krun_display_set_physical_size(
        ctx_id: u32,
        display_id: u32,
        width_mm: u16,
        height_mm: u16,
    ) -> i32;
    pub fn krun_display_set_refresh_rate(ctx_id: u32, display_id: u32, refresh_rate: u32) -> i32;
    pub fn krun_set_display_backend(
        ctx_id: u32,
        display_backend: *const c_void,
        backend_size: usize,
    ) -> i32;

    // ---- Audio --------------------------------------------------------------

    pub fn krun_set_snd_device(ctx_id: u32, enable: bool) -> i32;

    // ---- Resource limits + metadata -----------------------------------------

    pub fn krun_set_rlimits(ctx_id: u32, rlimits: *const *const c_char) -> i32;
    pub fn krun_set_smbios_oem_strings(ctx_id: u32, oem_strings: *const *const c_char) -> i32;

    // ---- Exec configuration -------------------------------------------------

    pub fn krun_set_workdir(ctx_id: u32, workdir_path: *const c_char) -> i32;
    pub fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;
    pub fn krun_set_env(ctx_id: u32, envp: *const *const c_char) -> i32;
    pub fn krun_setuid(ctx_id: u32, uid: uid_t) -> i32;
    pub fn krun_setgid(ctx_id: u32, gid: gid_t) -> i32;

    // ---- Firmware + kernel --------------------------------------------------

    pub fn krun_set_firmware(ctx_id: u32, firmware_path: *const c_char) -> i32;
    pub fn krun_set_kernel(
        ctx_id: u32,
        kernel_path: *const c_char,
        kernel_format: u32,
        initramfs: *const c_char,
        cmdline: *const c_char,
    ) -> i32;
    pub fn krun_set_root_disk_remount(
        ctx_id: u32,
        device: *const c_char,
        fstype: *const c_char,
        options: *const c_char,
    ) -> i32;

    // ---- TEE (Trusted Execution Environment) --------------------------------

    pub fn krun_set_tee_config_file(ctx_id: u32, filepath: *const c_char) -> i32;

    // ---- vsock --------------------------------------------------------------

    pub fn krun_add_vsock_port(ctx_id: u32, port: u32, c_filepath: *const c_char) -> i32;
    pub fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        c_filepath: *const c_char,
        listen: bool,
    ) -> i32;
    pub fn krun_add_vsock(ctx_id: u32, tsi_features: u32) -> i32;
    pub fn krun_disable_implicit_vsock(ctx_id: u32) -> i32;

    // ---- Console / serial ---------------------------------------------------

    pub fn krun_set_console_output(ctx_id: u32, c_filepath: *const c_char) -> i32;
    pub fn krun_disable_implicit_console(ctx_id: u32) -> i32;
    pub fn krun_set_kernel_console(ctx_id: u32, console_id: *const c_char) -> i32;
    pub fn krun_add_virtio_console_default(
        ctx_id: u32,
        input_fd: c_int,
        output_fd: c_int,
        err_fd: c_int,
    ) -> i32;
    pub fn krun_add_serial_console_default(ctx_id: u32, input_fd: c_int, output_fd: c_int) -> i32;
    pub fn krun_add_virtio_console_multiport(ctx_id: u32) -> i32;
    pub fn krun_add_console_port_tty(
        ctx_id: u32,
        console_id: u32,
        name: *const c_char,
        tty_fd: c_int,
    ) -> i32;
    pub fn krun_add_console_port_inout(
        ctx_id: u32,
        console_id: u32,
        name: *const c_char,
        input_fd: c_int,
        output_fd: c_int,
    ) -> i32;

    // ---- Virtualisation features --------------------------------------------

    pub fn krun_set_nested_virt(ctx_id: u32, enabled: bool) -> i32;
    pub fn krun_check_nested_virt() -> i32;
    pub fn krun_has_feature(feature: u64) -> i32;
    pub fn krun_get_max_vcpus() -> i32;
    pub fn krun_split_irqchip(ctx_id: u32, enable: bool) -> i32;

    // ---- Logging ------------------------------------------------------------

    pub fn krun_set_log_level(level: u32) -> i32;
    pub fn krun_init_log(target_fd: c_int, level: u32, style: u32, options: u32) -> i32;

    // ---- Shutdown signalling ------------------------------------------------

    pub fn krun_get_shutdown_eventfd(ctx_id: u32) -> i32;

    // ---- Boot ---------------------------------------------------------------
    //
    // Blocking — call on a dedicated OS thread, not a tokio task.

    pub fn krun_start_enter(ctx_id: u32) -> i32;
}
