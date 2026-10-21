# Supported platforms

| Platform         | Architecture  | Virtualisation       | Status              |
| ---------------- | ------------- | -------------------- | ------------------- |
| macOS 12+        | Apple Silicon | Hypervisor.framework | first-class         |
| Linux 5.10+      | x86_64        | KVM                  | first-class         |
| Linux 5.10+      | arm64         | KVM                  | first-class         |
| macOS Intel      | x86_64        | n/a                  | not supported       |
| Windows (native) | any           | n/a                  | not supported       |
| Windows (WSL2)   | x86_64        | KVM via WSL2         | community-supported |

The default `cargo build` (stub backend) compiles on any platform Rust supports. Real VM boot needs `--features krunvm` and a supported host (see [ADR-007](adr/007-platform-support.md)).

## Real microVMs (`--features krunvm`)

libkrun, its guest kernel (libkrunfw) and the in-guest agent are embedded in the ward binaries at build time. There is nothing to install on the host except a hypervisor: Hypervisor.framework on macOS (present on every supported Mac) or KVM on Linux (`/dev/kvm` readable by your user, see [rootless.md](rootless.md)).

```sh
cargo build --release --workspace --features ward-core/krunvm,ward-daemon/krunvm,ward-mcp/krunvm
```

The build needs network access once, to fetch the pinned libkrun release from GitHub, and `rustup` so it can add the Linux musl target for the guest agent. See the README for the offline override and the macOS signing step.

### Known limitations

- Volumes (`ward volume`) are created as ext4 images but not yet attached to guests; block devices need a libkrun build with block support ([#43](https://github.com/igorjs/ward/issues/43)). Creating a volume also needs `mkfs.ext4` on the host.
- `ward run-code` (language snippets) is not implemented. Use `ward exec`, or `ward_run` through ward-mcp.
- Bind mounts must point inside the daemon's allowed roots (`/home/`, `/tmp/`, `/var/lib/ward/`) unless `WARD_ALLOW_HOST_MOUNTS=1` is set on the daemon.
- macOS CI runners cannot start VMs, so the real-VM test suite runs on Linux/KVM in CI and on macOS locally.

## End-user install (post-v0.1.0)

Once v0.1.0 is published, the one-line installer resolves the latest release tarball, verifies the SHA-256, and installs binaries under `~/.ward/bin/`:

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```
