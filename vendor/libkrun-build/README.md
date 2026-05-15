# Vendored libkrun build pipeline

This directory contains the build recipe for the pre-built `libkrun` and
`libkrunfw` artefacts that ward bundles **into end-user release artefacts**.
The actual binaries are not committed to git — they are produced by
`.github/workflows/vendor-libkrun.yml` and published as GitHub Releases
under tags of the form `libkrun-v<version>`.

## Who consumes these artefacts

**End users** consume them indirectly: a release packaging workflow
(forthcoming) bundles `wardd`, `ward`, and the matching `libkrun.dylib` /
`libkrunfw.dylib` into per-platform self-contained installers (`.pkg`,
`.deb`, install.sh tarballs). End users download one artefact and never
run a second install command — the libkrun dependency lives inside the
release binary's rpath.

**Developers** do *not* consume these artefacts during `cargo build`. The
build-script attempt at downloading them ran into a structural problem:
cargo builds dependency build scripts before dependents, so ward-core's
build.rs couldn't prepare the environment for `krun-sys`. See
`ward-core/build.rs` for the full rationale. Developers install libkrun
once via their system package manager — see DEVELOPMENT.md at the repo root.

## Layout

```
vendor/libkrun-build/
├── README.md         this file
├── version.txt       single line: the libkrun version to build (e.g. 1.10.1)
├── build.sh          POSIX shell script that builds libkrun + libkrunfw
│                     for the host platform and emits a tarball with
│                     relocatable @rpath / $ORIGIN install names.
└── checksums.txt     SHA-256 sums of each per-target tarball. Populated
                      by humans after a vendor CI run completes; consumed
                      by the (future) release packaging workflow to gate
                      its bundling step against tampered downloads.
```

## How to bump libkrun

1. Edit `version.txt` to the new version (no `v` prefix, just the number).
2. Trigger `.github/workflows/vendor-libkrun.yml` from the Actions tab
   (workflow_dispatch). The matrix build takes ~15 min per target.
3. Once finished, the workflow publishes a release tagged `libkrun-v<new>`
   with one tarball per supported triple.
4. Pull the SHA-256 sums from the workflow's `release` job summary block,
   paste them into `checksums.txt`, commit.
5. The release packaging workflow (when it lands) will pick the new
   version up automatically.

## Supported targets

| Target triple                  | Notes                                 |
| ------------------------------ | ------------------------------------- |
| `aarch64-apple-darwin`         | macOS Apple Silicon (HVF backend)     |
| `x86_64-unknown-linux-gnu`     | Linux x86_64 (KVM backend)            |
| `aarch64-unknown-linux-gnu`    | Linux arm64 (KVM backend)             |

Adding more targets is a matter of extending the CI matrix and confirming
the libkrun upstream `make` build works on that platform.

## Why a build script (not a Rust crate that builds libkrun)

libkrun depends on `libkrunfw`, which builds a custom Linux kernel via
its own Makefile. Reimplementing that in a Rust `build.rs` would be
fragile and slow. Shelling out to libkrun's upstream `make all` keeps
us in lockstep with upstream and avoids re-litigating the kernel build.
