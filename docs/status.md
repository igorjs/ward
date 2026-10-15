# Status and roadmap

Live status, tickets, and priorities live on the [Ward project board](https://github.com/users/igorjs/projects/2). This page is a periodic snapshot.

- Stub backend: complete, 300+ tests passing
- libkrun FFI surface: complete (60 symbols, hand-maintained)
- VM lifecycle wiring (`krun_start_enter` + shutdown signalling): complete
- OCI image pull + unpack: complete
- Self-contained runtime: libkrun, libkrunfw and the guest agent are embedded in the binaries; one host process per microVM
- Guest agent (`ward-agent`): runs as the VM's init process; mounts shares, configures the network, serves exec over vsock
- Real-VM end-to-end tests run in CI on Linux/KVM (`vm-e2e`) and cover exec, env, mounts, snapshots, egress, isolation and ward-mcp
- Cross-sandbox pub/sub broker: complete (deny default + group routing)
- Egress: deny / allowlist / open enforced by the in-process smoltcp stack, with a private-address block
- Volumes: fixed-size ext4 images shipped; block-attach blocked on libkrun built with `--enable-blk` ([#43](https://github.com/igorjs/ward/issues/43))
- Snapshots: disk-level archive/restore + `from_snapshot` shipped; live checkpoint/restore tracked by [#29](https://github.com/igorjs/ward/issues/29)
- First signed release (v0.1.0): release smoke-test ([#4](https://github.com/igorjs/ward/issues/4)), then cut v0.1.0 ([#5](https://github.com/igorjs/ward/issues/5))
- Embedded / daemonless runtime mode: planned per [ADR-016](adr/016-embedded-mode-microvms.md) (daemon stays for fleet use; CLI default flips to embedded). Bundles userspace networking via `smoltcp` (closes [#32](https://github.com/igorjs/ward/issues/32)) and rootless operation on macOS + Linux.
