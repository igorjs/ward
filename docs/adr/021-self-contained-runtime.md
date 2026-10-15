# ADR-021: Self-Contained Runtime and One Host Process per MicroVM

- **Status:** Accepted
- **Date created:** 2026-10-15
- **Date modified:** 2026-10-15

## Context

ADR-003 chose libkrun and linked it from the host's shared libraries (`brew install libkrun libkrunfw`, or distro packages). That left two problems.

- A user could not download one binary and run it. They also needed libkrun and libkrunfw, in versions that match the FFI declarations.
- A sandbox booted inside the daemon process. Apple's Hypervisor.framework allows one VM per process, so a second sandbox failed (`krun_start_enter` errno 22). `krun_get_shutdown_eventfd` exists only in libkrun-efi, so a running VM could not be stopped without ending the process.

## Decision

1. **Embed the runtime.** `ward-bundle` downloads the pinned libkrun release at build time, verifies it against `vendor/libkrun-checksums.txt`, cross-compiles `ward-agent` as a static musl binary, and embeds all three (gzip) in the binaries. At first run ward unpacks them to `~/.ward/runtime/libs-<id>` and loads them with `dlopen` (`libloading`). `ward-core` no longer links libkrun.
2. **One host process per microVM.** The ward binary re-executes itself with `__ward-vm-shim`, reads a JSON `VmSpec` from stdin and enters the VM. The parent keeps the child's stdin open for the VM's lifetime. EOF means the parent is gone, so the child exits. Stopping a sandbox kills its child. Guest console output goes to the parent's stderr, not its stdout.
3. **The guest agent is the VM's init process.** It mounts virtiofs shares, configures `eth0` statically (10.0.2.15/24, gateway and DNS 10.0.2.2, served by the in-process smoltcp stack) and serves exec over vsock.
4. **Every release ships the real backend.** The stub backend stays the default for plain `cargo build`.

## Consequences

- Install is one download. Source builds need `rustup` and network access once.
- Sandboxes are isolated from each other and from the daemon at process level. A crashed VM cannot take the daemon down.
- macOS binaries need the `com.apple.security.hypervisor` and `disable-library-validation` entitlements, because the hardened runtime otherwise refuses the unpacked ad-hoc-signed libraries.
- The binaries embed libkrunfw (LGPL-2.1, with the GPL-2.0 Linux kernel). See `THIRD-PARTY-NOTICES.md` for the source offer.
- Rootfs files are copied from the image cache, not hardlinked: the guest writes in place and a shared inode leaked edits between sandboxes.
- Real-VM end-to-end tests run in CI on Linux/KVM. macOS runners cannot start VMs.

## Alternatives considered

- **Keep linking system libkrun.** Rejected: contradicts the one-binary requirement.
- **Static libkrun in the binary.** Rejected: the upstream builds ship no static library, and libkrunfw must stay a separate loadable object.
- **Several VMs in one process.** Rejected: blocked by Hypervisor.framework and by the missing shutdown eventfd.
