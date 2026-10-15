# Third-party notices

Ward is licensed under AGPL-3.0-only (see `LICENSE`). Release binaries embed
the components below. Their licenses permit commercial use and redistribution.

| Component | Version | License | Role |
|---|---|---|---|
| libkrun | see `vendor/libkrun-version.txt` | Apache-2.0 | microVM monitor, loaded at runtime |
| libkrunfw | same bundle | LGPL-2.1-only (library), GPL-2.0-only (the Linux kernel inside it) | guest kernel, loaded at runtime |
| musl libc | linked into `ward-agent` | MIT | static runtime of the in-guest agent |

Rust crates are limited by `deny.toml` to permissive licenses. A new license
must be reviewed for commercial-use impact before it is added there.

## Source for GPL and LGPL components

The Linux kernel and libkrunfw are separate works. Ward loads them at runtime
and does not link them into its own code. You may obtain their complete
corresponding source, and the scripts that build the bundled binaries, from
https://github.com/igorjs/libkrun-builds (release `libkrun-v<version>`, where
`<version>` is the value in `vendor/libkrun-version.txt`). The upstream
projects are https://github.com/containers/libkrun and
https://github.com/containers/libkrunfw.

You may replace the embedded libraries by setting `WARD_LIBKRUN_TARBALL` when
building Ward from source.
