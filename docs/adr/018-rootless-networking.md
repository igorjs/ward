# ADR-018: Rootless Networking Architecture

**Status:** Proposed
**Date:** 2026-06-04
**Authors:** Igor

## Context

[ADR-016](016-embedded-mode-microvms.md) committed ward to a rootless
posture. The original plan for sandbox networking (issue #32) was to use
libkrun's `krun_add_net_tap` to publish ports via host TAP devices. TAP
requires `CAP_NET_ADMIN` on Linux, which is incompatible with rootless
operation, and on macOS requires `utun` setup that also escalates.

A code audit of `vendor/include/libkrun.h` (and `ward-core`'s
`backend/krun_ffi.rs`) shows libkrun already exposes **four** networking
options. The decision is which one(s) ward should support — not, as
ADR-016 implied, whether to implement userspace networking from scratch.

### libkrun networking surface (1.18)

| libkrun FFI                | Backend                  | Root needed? | External binary needed? |
|----------------------------|--------------------------|--------------|--------------------------|
| `krun_add_net_tap`         | Host TAP device          | Yes (`CAP_NET_ADMIN`) | No                |
| `krun_add_net_unixstream`  | Unix stream socket       | No           | No                       |
| `krun_add_net_unixgram`    | Unix datagram socket     | No           | No                       |
| `krun_set_passt_fd`        | [passt](https://passt.top/) | No        | Yes (`passt` daemon)     |
| `krun_set_gvproxy_path`    | [gvproxy](https://github.com/containers/gvisor-tap-vsock) | No | Yes (`gvproxy` binary) |

The two FD/socket options (`unixstream`, `unixgram`) terminate guest
traffic at a host socket — useful for *intra-host* routing but not for
giving the guest internet egress. The three remaining options are the
real candidates.

## Candidates

### Option A — TAP devices (`krun_add_net_tap`)

The microsandbox-comparison reference plan. Standard, full Linux TCP/IP
stack on the host. Hard requirement for root or `CAP_NET_ADMIN`. **Rejected**
by ADR-016's rootless commitment.

### Option B — passt (`krun_set_passt_fd`)

[passt](https://passt.top/) is a stateful user-mode TCP/IP translator
written in C. Connects to the guest over a socket pair, translates TCP/UDP
into host-side socket(2) calls. Available on most Linux distros (`apt
install passt`, `dnf install passt`), and from Homebrew on macOS.

- **Pros:** Battle-tested (default rootless networking for Podman, since
  Podman ~4.4). Pure userspace. Translates the guest's "I want to talk to
  1.1.1.1:443" into a host `connect(1.1.1.1, 443)` so the host's normal
  routing/firewall applies. Port forwarding is a passt command-line flag.
- **Cons:** External binary that ward must spawn and supervise.
  Architecture-specific (passt itself needs to support the guest arch's
  byte order; that's done, but adds a moving piece).

### Option C — gvproxy (`krun_set_gvproxy_path`)

[gvproxy](https://github.com/containers/gvisor-tap-vsock) is the
gVisor-derived userspace network stack, written in Go. Used by Podman and
podman-machine on macOS for the same rootless reason. Talks to the guest
via vsock or a Unix socket.

- **Pros:** Cross-platform (designed for macOS + Linux). Native vsock
  transport, which we already use for `ward-agent`. Larger feature set
  than passt (port forwarding, DNS, DHCP, mDNS).
- **Cons:** Go runtime in the dependency chain. Adds ~10 MB to the
  release artefact if bundled. Heavier than passt.

### Option D — smoltcp (rolled in-process)

[smoltcp](https://github.com/smoltcp-rs/smoltcp) is a pure-Rust,
no-external-deps TCP/IP stack. The original ADR-016 framing assumed we'd
implement userspace networking on top of smoltcp ourselves, terminating
the guest's virtio-net packets in a ward-owned smoltcp `Interface` and
proxying flows to host sockets.

- **Pros:** No external binary. Pure Rust, in-tree code. Fits SLSA L3 and
  reproducible-build posture without a third dependency. Library, not a
  daemon — embedded mode (`ward-mcp`) doesn't fork anything.
- **Cons:** Significant engineering. The host-side glue (virtio-net frame
  parsing, flow tracking, connection table, DNS) is real work. passt
  and gvproxy are years of hardening that we'd be reinventing.

## Decision

**Multi-backend strategy. Default = passt. Fallback = gvproxy. Long-term
optional = smoltcp.**

Concretely:

1. **Default (v0.1.x):** `passt` via `krun_set_passt_fd`.
   - `wardd` and `ward-runtime` detect `passt` on `$PATH` at startup.
     If missing, surface a clear error pointing the user at `apt install
     passt` / `brew install passt`.
   - Spawn one `passt` per sandbox (cheap, ~3 MB RSS).
   - Forward the FD to libkrun.
   - Port-publishing translates to `passt --tcp-ports` flags.
2. **Optional (v0.2):** `gvproxy` via `krun_set_gvproxy_path`.
   - Same probe-then-spawn pattern, gated by a `WARD_NETWORK_BACKEND=gvproxy`
     env var.
   - Useful for users who already have podman-machine and want to reuse
     the same gvproxy daemon.
3. **Research, not blocking (v0.3+):** smoltcp.
   - A `ward-net` crate that wraps the libkrun unixgram FD,
     parses virtio-net frames into smoltcp `RxToken`s, and proxies flows
     via host sockets. Avoids the external-binary dependency entirely.
   - Scope: documented in this ADR's "Future work" section. Not on the
     v0.1 path.

### Why passt and not gvproxy at v0.1

Three reasons:

- **Lighter.** passt is ~30 KB of native code; gvproxy ships a Go
  runtime.
- **Simpler FFI.** `krun_set_passt_fd(ctx, fd)` versus gvproxy's path-based
  setup, which requires ward to launch and lifecycle the gvproxy process
  and inject its socket path at sandbox boot. passt's `passt --fd` mode
  hands ward a pre-connected FD; libkrun just owns it.
- **Distro coverage.** passt is in Debian/Ubuntu stable, Fedora, Arch,
  and Homebrew. gvproxy is mostly via Podman packages today.

### Why support multi-backend at all

If passt becomes unmaintained, or a user hits an edge case (e.g. needs IPv6
broadcast across guests), having gvproxy as a no-code-change opt-out is
worth the small `enum NetworkBackend { Passt, Gvproxy }` plumbing.

### Why smoltcp is *not* default

Engineering cost. The audit found rootless networking is **already
shipped** by libkrun via passt/gvproxy. Rebuilding that in smoltcp is
~weeks of work for a quality bar that's already met externally. Keep
it on the research backlog; revisit if external binaries become a
distribution problem (e.g. mobile-style deployments, scratch container
images).

## Implementation

1. **New module `ward-core/src/network/mod.rs`** — small abstraction
   trait `NetworkBackend` with one impl per option.
2. **`ward-core::network::PasstBackend`** —
   - Probe binary via `which::which("passt")`.
   - On sandbox create: `Command::new("passt").args([...])
       .stdin(Stdio::null()).stdout(Stdio::null()).stderr(piped)`,
     capture the FD passt opens on stdin (via socketpair, see passt(1)),
     hand it to libkrun via `krun_set_passt_fd`.
   - On sandbox remove: SIGTERM the passt child, then SIGKILL after
     timeout.
3. **Port publishing** — sandbox create options gain a `publish_ports:
   Vec<(u16, u16)>` field (host, guest). Translated to `--tcp-ports
   host:guest,...` on the passt command line.
4. **Backend selection** — `WARD_NETWORK_BACKEND` env var with values
   `passt` (default), `gvproxy`, `none` (skip entirely; the guest has no
   network).
5. **Tests** — unit tests cover the command-line translation and FD
   plumbing. Integration tests are gated on `passt` being installed
   (skip otherwise with a clear `eprintln!`).

## Consequences

### Positive

- **Issue #32 closes** with a smaller, working solution than the
  original TAP plan.
- **Rootless ships at v0.1.** The CI matrix already passes without
  root for stub-backend tests; the passt path matches.
- **Multi-backend lays the groundwork for ADR-014 (WASM backend).**
  Per-backend `NetworkBackend` impls keep the network plumbing
  isolated from the sandbox lifecycle.

### Negative

- **External binary dependency.** Users must install `passt`. `install.sh`
  surfaces this as a post-install hint; CI matrices add a `apt install
  passt` / `brew install passt` step.
- **Per-sandbox process.** One passt per sandbox is fine but adds N
  processes to the host's process table. For very large fleets this is
  worth reviewing.

### Neutral

- **smoltcp is not abandoned, just deferred.** If passt/gvproxy turn out
  to be problematic in operation, the smoltcp route is documented and
  the research crate (`ward-net`) can be picked up.

## Future work — `ward-net` smoltcp prototype

When (if) we move to pure-Rust networking:

- New crate `ward-net` (AGPL, alongside `ward-runtime`).
- `ward-net::TcpProxy` owns a smoltcp `Interface` with a `RawDevice` that
  reads from / writes to a `libkrun_set_net_fd` socket.
- A flow table maps `(guest_ip, guest_port, dst_ip, dst_port)` to a host
  `tokio::net::TcpStream`. Per-flow tasks pump bytes both ways.
- DNS, DHCP, and ICMP echo handled in-stack.
- Port forwarding handled by binding a host `TcpListener` and connecting
  inbound flows into the smoltcp side.

Out of scope for v0.1.

## References

- ADR-016: embedded-mode microVMs (motivates the rootless commitment)
- Issue [#32](https://github.com/igorjs/ward/issues/32): closes in
  favour of this ADR's passt default
- [passt(1)](https://passt.top/passt/about/) — design doc
- [gvproxy](https://github.com/containers/gvisor-tap-vsock) — README
- [smoltcp](https://github.com/smoltcp-rs/smoltcp) — README + book
- libkrun's `bindings/c/include/libkrun.h` for the FFI surface this
  ADR builds on
