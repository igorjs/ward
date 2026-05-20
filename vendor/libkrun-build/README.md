# Vendored libkrun build pipeline

This directory contains the build recipe for the pre-built `libkrun` and
`libkrunfw` artefacts that ward bundles. The actual binaries are **not**
committed to git — they are produced by `.github/workflows/vendor-libkrun.yml`
and published as GitHub Releases under tags of the form `libkrun-v<version>`.

At build time, `ward-core/build.rs` downloads the matching tarball for the
target triple from those releases, verifies its SHA-256, and configures the
linker so users never run anything but `ward` itself. There is no
`brew install`, no `apt-get install libkrun-dev` — the dependency lives
inside ward's own release cadence.

## Layout

```
vendor/libkrun-build/
├── README.md         this file
├── version.txt       single line: the libkrun version to build (e.g. 1.10.1)
├── build.sh          POSIX shell script that builds libkrun + libkrunfw
│                     for the host platform and emits a tarball.
└── checksums.txt     SHA-256 sums of each per-target tarball. Populated by
                      humans after a vendor CI run completes; committed back
                      to gate build.rs against tampered downloads.
```

## How to bump libkrun

1. Edit `version.txt` to the new version (no `v` prefix, just the number).
2. Push the change. The path-filtered workflow `.github/workflows/vendor-libkrun.yml`
   detects the diff and triggers automatically. (Or trigger it manually via
   `workflow_dispatch` if you didn't push yet.)
3. Wait for the matrix build to finish (~15 min per target). It publishes
   a release tagged `libkrun-v<new-version>` with one tarball per supported
   triple.
4. Pull the SHA-256 sums from the workflow's `Artefacts` step output, paste
   them into `checksums.txt`, commit.
5. `ward-core/build.rs` will pick up the new version on next build because
   it reads `version.txt` directly.

## Supported targets

| Target triple                  | Notes                                 |
| ------------------------------ | ------------------------------------- |
| `aarch64-apple-darwin`         | macOS Apple Silicon (HVF backend)     |
| `x86_64-unknown-linux-gnu`     | Linux x86_64 (KVM backend)            |
| `aarch64-unknown-linux-gnu`    | Linux arm64 (KVM backend)             |

Adding more targets is a matter of extending the CI matrix and confirming
the libkrun upstream `make` build works on that platform.

## Why this isn't `brew install`

The whole point of ward is "user installs nothing but ward". A separate
system-install step would violate that for both users *and* developers.
This pipeline is the technical answer: ward's own CI builds libkrun once
per version, ships the binaries via GitHub Releases, and ward's build
script fetches them transparently. Same code path for `cargo install ward`,
for the eventual `.pkg` installer, and for `brew install ward` — none of
those routes ever require the user to know libkrun exists.

## Local dev workflow

You almost never need to touch anything here. The flow is:

1. Clone ward.
2. Run `cargo build --features krunvm`.
3. `ward-core/build.rs` downloads the matching libkrun tarball into
   `target/<profile>/build/ward-core-*/out/libkrun/`, verifies its
   checksum, and configures linking.

If the workflow has never run for your version (i.e. no GitHub Release
exists yet at `libkrun-v<version>`), the build fails with a clear error
telling you to trigger the workflow. That's the only error path that
requires human action.

## Why a build script (not a Rust crate that builds libkrun)?

libkrun itself depends on `libkrunfw`, which builds a custom Linux kernel
via its own Makefile. Reimplementing that in a Rust `build.rs` would be
fragile and slow. Shelling out to libkrun's upstream `make all` keeps us
in lockstep with upstream and avoids re-litigating the kernel build.
