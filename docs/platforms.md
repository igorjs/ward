# Supported platforms

| Platform         | Architecture  | Virtualisation       | Status              |
| ---------------- | ------------- | -------------------- | ------------------- |
| macOS 12+        | Apple Silicon | Hypervisor.framework | first-class         |
| Linux 5.10+      | x86_64        | KVM                  | first-class         |
| Linux 5.10+      | arm64         | KVM                  | first-class         |
| macOS Intel      | x86_64        | n/a                  | not supported       |
| Windows (native) | any           | n/a                  | not supported       |
| Windows (WSL2)   | x86_64        | KVM via WSL2         | community-supported |

The default `cargo build` (stub backend) compiles on any platform Rust
supports. Real VM boot needs `--features krunvm` and a supported host
(see [ADR-007](adr/007-platform-support.md)).

## Real microVMs (`--features krunvm`)

Install libkrun + libkrunfw, then build with the feature flag:

### macOS Apple Silicon (12+)

```sh
brew tap slp/krun
brew install slp/krun/libkrun slp/krun/libkrunfw
cargo build --release --features krunvm
```

### Linux (Debian/Ubuntu, kernel 5.10+ with KVM)

```sh
# Follow https://github.com/containers/libkrun#installing
sudo apt-get install -y libkrun-dev libkrunfw-dev
cargo build --release --features krunvm
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the full setup matrix and
build-time gotchas.

## End-user install (post-v0.1.0)

Once v0.1.0 is published, the one-line installer resolves the latest
release tarball, verifies the SHA-256, and installs binaries under
`~/.ward/bin/`:

```sh
curl -fsSL https://raw.githubusercontent.com/igorjs/ward/main/install.sh | bash
```
