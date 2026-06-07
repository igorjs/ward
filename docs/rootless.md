# Running ward rootless

ward is designed to run without root or `sudo`. This page documents the
platform prerequisites that make rootless operation possible, the
permissions ward does *not* need, and how to verify your setup.

## Why rootless

Per [ADR-016](adr/016-embedded-mode-microvms.md), ward avoids root for
three reasons:

- **SDK distribution.** Users installing via `cargo`, `pip`, `npm` won't
  `sudo`-install a runtime. Rootless makes that path viable.
- **Multi-user host safety.** Each user runs their own `wardd` against
  their own data dir. No shared system daemon.
- **Reduced blast radius.** A compromised sandbox or supervisor can't
  reach beyond the user account that owns it.

## What ward does *not* require

- No `setuid` binaries.
- No `CAP_NET_ADMIN`, `CAP_NET_BIND_SERVICE`, or `CAP_SYS_ADMIN`.
- No write access to `/etc`, `/var`, `/usr`, or any system directory.
- No `sudo` to install (see [install.sh](../install.sh)).

Everything ward creates lives in `$HOME/.ward/` (or `$WARD_DATA_DIR`)
with `0700` permissions enforced by umask.

## Platform prerequisites

### macOS (Apple Silicon)

libkrun on macOS uses the Hypervisor framework, which requires a
binary entitlement. ward's release artefacts ship pre-signed with this
entitlement; building from source needs:

1. Xcode command-line tools (`xcode-select --install`).
2. libkrun installed via `brew install slp/krun/libkrun
   slp/krun/libkrunfw`.
3. The build sets `com.apple.security.hypervisor` on `wardd` and the
   embedded `ward-mcp` binary. CI handles this; for local dev, sign
   manually:

```sh
codesign --sign - --entitlements ward-daemon/entitlements.plist --force \
  target/release/wardd
```

If the entitlement is missing, libkrun returns
`HV_ERROR_BAD_ARGUMENT` at boot — diagnose with:

```sh
codesign -d --entitlements - target/release/wardd
```

No `sudo` is needed at any step.

### Linux

libkrun uses KVM, which requires access to `/dev/kvm`. The standard
distro convention is the `kvm` group:

```sh
# Verify your access
ls -l /dev/kvm
# crw-rw---- 1 root kvm 10, 232 ...   ← group=kvm

# If you're not in the group:
sudo usermod -aG kvm $USER
# Then log out and back in for the group to take effect.
```

That `sudo usermod` is the one-time setup step. After that, `wardd`
runs as your normal user.

Optional: for rootless networking via `passt` (per
[ADR-018](adr/018-rootless-networking.md)), install `passt`:

```sh
sudo apt install passt        # Debian/Ubuntu
sudo dnf install passt        # Fedora
sudo pacman -S passt          # Arch
```

`passt` is required only when the daemon needs to give sandboxes
network egress. Stub-backend tests don't need it.

## Verifying rootless

```sh
# 1. Confirm wardd is not setuid
ls -l "$(which wardd)"
# -rwx------ 1 you you ...   ← no `s` in the perms

# 2. Confirm wardd starts as your normal user
wardd &
ps -o user,pid,comm -p $!
# you  12345 wardd

# 3. Confirm sandboxes work without sudo
ward create alpine
# id: ...
```

If any of these surprise you (e.g. `wardd` started but won't create a
sandbox because of permission denied), see
[`docs/platforms.md`](platforms.md) for platform-specific debugging.

## Common pitfalls

- **macOS: "permission denied" opening Hypervisor.** Re-sign the binary
  with the entitlement (above). Apple revokes ad-hoc signatures across
  certain system updates.
- **Linux: `open /dev/kvm: permission denied`.** You're not in the
  `kvm` group, or you haven't logged out since being added.
- **Linux: passt missing.** Install via your package manager. ward
  fails fast with a hint pointing here.

## Future work

- macOS notarization for the signed `wardd` / `ward-mcp` binaries so
  Gatekeeper accepts them without `xattr -d com.apple.quarantine`.
  Tracked as [#30](https://github.com/igorjs/ward/issues/30).
- A `ward doctor` subcommand that runs the verification above
  automatically. (Not filed yet — file an issue if you want it.)
