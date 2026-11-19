// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! A second layer inside the guest for every workload process: a smaller
//! capability bounding set, no_new_privs and a seccomp filter that refuses
//! dangerous system calls with EPERM. The microVM stays the boundary. This
//! is a deny list, so ordinary tools (shells, python, node, git, package
//! managers) keep working.
//!
//! The filter builder is plain data, so it is tested on every host. Only
//! installing it needs Linux.

/// The kernel's `struct sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JGE_K: u16 = 0x35;
const BPF_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;

/// Offsets into the kernel's `struct seccomp_data`.
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const DATA_ARGS: u32 = 16;
const DATA_ARG_SIZE: u32 = 8;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const ERRNO_PERM: u32 = 1;
const ERRNO_NOSYS: u32 = 38;

/// `CLONE_NEWUSER`, the flag that makes a user namespace.
const CLONE_NEWUSER: u32 = 0x1000_0000;

/// What a matching rule answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Fail the call with EPERM.
    Deny,
    /// Fail the call with ENOSYS, so callers fall back to an older call.
    NoSys,
}

/// One system call to refuse, optionally only when an argument has a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub nr: u32,
    /// `(argument index, mask)`: match only when the low 32 bits hold any
    /// bit of the mask.
    pub arg_flag: Option<(u32, u32)>,
    pub verdict: Verdict,
}

impl Rule {
    const fn deny(nr: i64) -> Self {
        Self {
            nr: nr as u32,
            arg_flag: None,
            verdict: Verdict::Deny,
        }
    }

    const fn deny_flag(nr: i64, arg: u32, mask: u32) -> Self {
        Self {
            nr: nr as u32,
            arg_flag: Some((arg, mask)),
            verdict: Verdict::Deny,
        }
    }
}

/// The ABI the filter guards.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub audit_arch: u32,
    /// Numbers from here up are another ABI (x32), refused outright.
    pub deny_from_nr: Option<u32>,
}

/// Where a jump lands, filled in once the layout is known.
#[derive(Clone, Copy)]
enum Dest {
    Deny,
    NoSys,
    Kill,
}

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Build the filter: other ABIs are killed, listed calls get their verdict,
/// everything else is allowed.
///
/// # Errors
///
/// Fails when the rule list is too long for the jump width of classic BPF.
pub fn build_program(target: Target, rules: &[Rule]) -> Result<Vec<SockFilter>, String> {
    let mut prog = vec![stmt(BPF_LD_W_ABS, DATA_ARCH)];
    // (instruction, true target, false target); None keeps the jump as is.
    let mut fixups: Vec<(usize, Option<Dest>, Option<Dest>)> = Vec::new();

    prog.push(jump(BPF_JEQ_K, target.audit_arch, 0, 0));
    fixups.push((1, None, Some(Dest::Kill)));
    prog.push(stmt(BPF_LD_W_ABS, DATA_NR));
    if let Some(from) = target.deny_from_nr {
        fixups.push((prog.len(), Some(Dest::Deny), None));
        prog.push(jump(BPF_JGE_K, from, 0, 0));
    }
    for rule in rules {
        let dest = match rule.verdict {
            Verdict::Deny => Dest::Deny,
            Verdict::NoSys => Dest::NoSys,
        };
        match rule.arg_flag {
            None => {
                fixups.push((prog.len(), Some(dest), None));
                prog.push(jump(BPF_JEQ_K, rule.nr, 0, 0));
            }
            Some((arg, mask)) => {
                // Skip the block unless the number matches, then test the
                // flag, then reload the number for the next rule.
                prog.push(jump(BPF_JEQ_K, rule.nr, 0, 3));
                prog.push(stmt(BPF_LD_W_ABS, DATA_ARGS + arg * DATA_ARG_SIZE));
                fixups.push((prog.len(), Some(dest), None));
                prog.push(jump(BPF_JSET_K, mask, 0, 0));
                prog.push(stmt(BPF_LD_W_ABS, DATA_NR));
            }
        }
    }

    let allow = prog.len();
    prog.push(stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    prog.push(stmt(BPF_RET_K, SECCOMP_RET_ERRNO | ERRNO_PERM));
    prog.push(stmt(BPF_RET_K, SECCOMP_RET_ERRNO | ERRNO_NOSYS));
    prog.push(stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));
    let index_of = |dest: Dest| match dest {
        Dest::Deny => allow + 1,
        Dest::NoSys => allow + 2,
        Dest::Kill => allow + 3,
    };
    let offset = |from: usize, dest: Dest| {
        u8::try_from(index_of(dest) - from - 1).map_err(|_| "seccomp rule list is too long")
    };
    for (at, jt, jf) in fixups {
        if let Some(dest) = jt {
            prog[at].jt = offset(at, dest)?;
        }
        if let Some(dest) = jf {
            prog[at].jf = offset(at, dest)?;
        }
    }
    Ok(prog)
}

/// Capabilities a workload keeps in its bounding set, Docker's default set.
/// Everything else (sys_admin, sys_time, sys_module, sys_boot, sys_ptrace,
/// net_admin and the rest) is dropped for good.
#[cfg(target_os = "linux")]
const KEPT_CAPS: [u32; 14] = [
    0,  // CHOWN
    1,  // DAC_OVERRIDE
    3,  // FOWNER
    4,  // FSETID
    5,  // KILL
    6,  // SETGID
    7,  // SETUID
    8,  // SETPCAP
    10, // NET_BIND_SERVICE
    13, // NET_RAW
    18, // SYS_CHROOT
    27, // MKNOD
    29, // AUDIT_WRITE
    31, // SETFCAP
];

/// Highest capability number the loop will try. The kernel has fewer.
#[cfg(target_os = "linux")]
const CAP_SCAN_LIMIT: u32 = 63;

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        CAP_SCAN_LIMIT, CLONE_NEWUSER, KEPT_CAPS, Rule, SockFilter, Target, Verdict, build_program,
    };
    use crate::user::{Step, tag};
    use std::io;

    #[cfg(target_arch = "x86_64")]
    const TARGET: Target = Target {
        audit_arch: 0xc000_003e,
        deny_from_nr: Some(0x4000_0000),
    };
    #[cfg(target_arch = "aarch64")]
    const TARGET: Target = Target {
        audit_arch: 0xc000_00b7,
        deny_from_nr: None,
    };

    // Missing from libc on some targets.
    #[cfg(target_arch = "x86_64")]
    pub(super) const SYS_KEXEC_FILE_LOAD: i64 = 320;
    #[cfg(target_arch = "aarch64")]
    pub(super) const SYS_KEXEC_FILE_LOAD: i64 = 294;

    /// The system calls a workload may not make.
    pub(super) fn default_rules() -> Vec<Rule> {
        let nrs = [
            // Clock changes.
            libc::SYS_clock_settime,
            libc::SYS_clock_adjtime,
            libc::SYS_settimeofday,
            libc::SYS_adjtimex,
            // Kernel and boot.
            libc::SYS_kexec_load,
            SYS_KEXEC_FILE_LOAD,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_reboot,
            libc::SYS_swapon,
            libc::SYS_swapoff,
            // Mounts.
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_mount_setattr,
            // Kernel attack surface and key material.
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            // Runtimes (libuv, liburing users) fall back when it fails.
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ];
        let mut rules: Vec<Rule> = nrs.into_iter().map(Rule::deny).collect();
        // User namespaces hand out full capabilities inside them.
        rules.push(Rule::deny_flag(libc::SYS_unshare, 0, CLONE_NEWUSER));
        rules.push(Rule::deny_flag(libc::SYS_setns, 1, CLONE_NEWUSER));
        rules.push(Rule::deny_flag(libc::SYS_clone, 0, CLONE_NEWUSER));
        // clone3 keeps its flags in a struct the filter cannot read. ENOSYS
        // sends libc back to clone, which the rule above checks.
        rules.push(Rule {
            nr: libc::SYS_clone3 as u32,
            arg_flag: None,
            verdict: Verdict::NoSys,
        });
        rules
    }

    /// The filter, built once in the parent so the child allocates nothing.
    #[derive(Clone)]
    pub struct Hardening {
        pub(super) filter: Vec<SockFilter>,
    }

    impl Hardening {
        /// Build the default filter.
        ///
        /// # Errors
        ///
        /// Fails when the rule list does not fit classic BPF.
        pub fn new() -> Result<Self, String> {
            Ok(Self {
                filter: build_program(TARGET, &default_rules())?,
            })
        }

        /// Drop capabilities, set no_new_privs and install the filter. Meant
        /// for the child between fork and exec: it does not allocate. Any
        /// failure is returned so the workload never runs half hardened.
        pub fn apply(&self) -> io::Result<()> {
            drop_bounding_caps().map_err(|e| tag(Step::CapDrop, e))?;
            // SAFETY: plain prctl calls with integer arguments.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                return Err(tag(Step::NoNewPrivs, io::Error::last_os_error()));
            }
            let prog = libc::sock_fprog {
                len: u16::try_from(self.filter.len()).map_err(|_| io::ErrorKind::InvalidInput)?,
                filter: self.filter.as_ptr().cast_mut().cast(),
            };
            // SAFETY: `prog` points at a live filter of `len` instructions
            // for the duration of the call, and the kernel copies it.
            let rc = unsafe {
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    std::ptr::addr_of!(prog),
                    0,
                    0,
                )
            };
            if rc != 0 {
                return Err(tag(Step::Seccomp, io::Error::last_os_error()));
            }
            Ok(())
        }
    }

    /// Whether the guest kernel can install a seccomp filter.
    pub fn seccomp_available() -> bool {
        // SAFETY: PR_GET_SECCOMP takes no arguments; EINVAL means no seccomp.
        unsafe { libc::prctl(libc::PR_GET_SECCOMP) >= 0 }
    }

    fn drop_bounding_caps() -> io::Result<()> {
        for cap in 0..=CAP_SCAN_LIMIT {
            if KEPT_CAPS.contains(&cap) {
                continue;
            }
            // SAFETY: plain prctl call with integer arguments.
            if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(cap), 0, 0, 0) } != 0
            {
                let err = io::Error::last_os_error();
                // EINVAL marks the first number past the kernel's last cap.
                if err.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(err);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{Hardening, seccomp_available};

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TARGET: Target = Target {
        audit_arch: 0xc000_003e,
        deny_from_nr: Some(0x4000_0000),
    };
    const NR_DENIED: u32 = 100;
    const NR_FLAGGED: u32 = 200;
    const NR_NOSYS: u32 = 300;
    const NR_PLAIN: u32 = 5;
    const FLAG: u32 = 0x10;

    /// Run the program like the kernel would and return its verdict.
    fn run(prog: &[SockFilter], arch: u32, nr: u32, args: [u64; 6]) -> u32 {
        let mut acc = 0u32;
        let mut pc = 0usize;
        loop {
            let i = prog[pc];
            pc += 1;
            let jump = |hit: bool| usize::from(if hit { i.jt } else { i.jf });
            match i.code {
                BPF_LD_W_ABS => {
                    acc = match i.k {
                        DATA_NR => nr,
                        DATA_ARCH => arch,
                        k => {
                            let n = ((k - DATA_ARGS) / DATA_ARG_SIZE) as usize;
                            args[n] as u32
                        }
                    };
                }
                BPF_JEQ_K => pc += jump(acc == i.k),
                BPF_JGE_K => pc += jump(acc >= i.k),
                BPF_JSET_K => pc += jump(acc & i.k != 0),
                BPF_RET_K => return i.k,
                other => panic!("unexpected opcode {other:#x}"),
            }
        }
    }

    fn program() -> Vec<SockFilter> {
        let rules = [
            Rule::deny(i64::from(NR_DENIED)),
            Rule::deny_flag(i64::from(NR_FLAGGED), 1, FLAG),
            Rule {
                nr: NR_NOSYS,
                arg_flag: None,
                verdict: Verdict::NoSys,
            },
            // A second flag rule after the first proves the reload works.
            Rule::deny(i64::from(NR_DENIED + 1)),
        ];
        build_program(TEST_TARGET, &rules).unwrap()
    }

    fn verdict(nr: u32, args: [u64; 6]) -> u32 {
        run(&program(), TEST_TARGET.audit_arch, nr, args)
    }

    const NO_ARGS: [u64; 6] = [0; 6];
    const EPERM_VERDICT: u32 = SECCOMP_RET_ERRNO | ERRNO_PERM;

    #[test]
    fn given_a_listed_call_when_run_then_it_fails_with_eperm() {
        assert_eq!(verdict(NR_DENIED, NO_ARGS), EPERM_VERDICT);
        assert_eq!(verdict(NR_DENIED + 1, NO_ARGS), EPERM_VERDICT);
    }

    #[test]
    fn given_an_unlisted_call_when_run_then_it_is_allowed() {
        assert_eq!(verdict(NR_PLAIN, NO_ARGS), SECCOMP_RET_ALLOW);
    }

    #[test]
    fn given_a_flag_rule_when_the_flag_is_set_then_it_is_denied_else_allowed() {
        let with = [0, u64::from(FLAG) | 1 << 40, 0, 0, 0, 0];
        let without = [FLAG.into(), 0x20, 0, 0, 0, 0];
        assert_eq!(verdict(NR_FLAGGED, with), EPERM_VERDICT);
        assert_eq!(verdict(NR_FLAGGED, without), SECCOMP_RET_ALLOW);
    }

    #[test]
    fn given_a_nosys_rule_when_run_then_it_fails_with_enosys() {
        assert_eq!(verdict(NR_NOSYS, NO_ARGS), SECCOMP_RET_ERRNO | ERRNO_NOSYS);
    }

    #[test]
    fn given_another_abi_when_run_then_the_process_is_killed() {
        let prog = program();
        assert_eq!(
            run(&prog, 0x4000_0003, NR_PLAIN, NO_ARGS),
            SECCOMP_RET_KILL_PROCESS
        );
        assert_eq!(
            run(
                &prog,
                TEST_TARGET.audit_arch,
                0x4000_0000 + NR_PLAIN,
                NO_ARGS
            ),
            EPERM_VERDICT
        );
    }

    #[test]
    fn given_too_many_rules_when_built_then_it_fails_instead_of_wrapping() {
        let rules: Vec<Rule> = (0..300).map(Rule::deny).collect();
        assert!(build_program(TEST_TARGET, &rules).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_the_default_rules_when_run_then_dangerous_calls_are_denied_and_common_ones_allowed() {
        let prog = Hardening::new().unwrap().filter;
        let arch = linux_arch();
        let go = |nr: i64, args: [u64; 6]| run(&prog, arch, nr as u32, args);
        for nr in [
            libc::SYS_clock_settime,
            libc::SYS_settimeofday,
            libc::SYS_adjtimex,
            libc::SYS_mount,
            libc::SYS_bpf,
            libc::SYS_init_module,
            libc::SYS_reboot,
        ] {
            assert_eq!(go(nr, NO_ARGS), EPERM_VERDICT, "syscall {nr}");
        }
        let newuser = u64::from(CLONE_NEWUSER);
        assert_eq!(
            go(libc::SYS_unshare, [newuser, 0, 0, 0, 0, 0]),
            EPERM_VERDICT
        );
        assert_eq!(go(libc::SYS_unshare, NO_ARGS), SECCOMP_RET_ALLOW);
        assert_eq!(go(libc::SYS_clone, [newuser, 0, 0, 0, 0, 0]), EPERM_VERDICT);
        assert_eq!(
            go(libc::SYS_clone3, NO_ARGS),
            SECCOMP_RET_ERRNO | ERRNO_NOSYS
        );
        for nr in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_execve,
            libc::SYS_clock_gettime,
            libc::SYS_ptrace,
            libc::SYS_chroot,
        ] {
            assert_eq!(go(nr, NO_ARGS), SECCOMP_RET_ALLOW, "syscall {nr}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_the_kexec_file_load_number_when_checked_then_it_matches_the_kernel_table() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(super::linux::SYS_KEXEC_FILE_LOAD, 320);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(super::linux::SYS_KEXEC_FILE_LOAD, 294);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn linux_arch() -> u32 {
        0xc000_003e
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    fn linux_arch() -> u32 {
        0xc000_00b7
    }
}
