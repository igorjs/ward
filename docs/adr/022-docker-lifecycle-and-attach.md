# ADR-022: Docker-Style Lifecycle, Attach Streaming and Persisted Sandboxes

- **Status:** Accepted
- **Date created:** 2026-10-19
- **Date modified:** 2026-10-19

## Context

Ward sandboxes are create-and-destroy only. The CLI should work like Docker's: stop and start a sandbox, run a main command, attach to it, copy files, publish ports. That needs decisions the code cannot take back cheaply, because `ward.proto` is public API (ADR-004) and `meta.json` is an on-disk format.

- A sandbox must survive a stop, a start and a daemon restart, so its state and options must be on disk.
- `exec` today streams output lines only. An interactive shell needs stdin, a pty, resize and signals.
- `cp` needs a safe way to move files, and the guest is untrusted.
- Persisting a sandbox means persisting its `env`, which can carry secrets, and its authorization token.

## Decision

1. **Lifecycle state machine** (both sandbox kinds).

   | State | Meaning | Allowed next |
   |---|---|---|
   | creating | rootfs being prepared or VM booting | running, failed |
   | running | VM up | stopping, exited, failed |
   | exited | the main process ended (only for sandboxes created with a command) | creating (on start), stopping, removing |
   | stopping | stop requested, VM going away | stopped, failed |
   | stopped | VM gone, rootfs kept | creating (on start), removing |
   | failed | boot, start or stop failed | removing, creating (retry start) |

   - `start` moves a stopped or exited sandbox back through `creating`. There is no starting state.
   - `stop` sends SIGTERM to the main process through the agent, waits `STOP_GRACE_SECONDS` (10), then ends the VM. A main process that ends while the sandbox is `stopping` is recorded as `stopped`, never `exited`. `exit_code` is 143 on SIGTERM and 137 if forced.
   - The VM stays up in `exited`. `start` or `restart` there first ends the VM host process, so only one exists.
   - A sandbox created without a command has no main process, never becomes `exited`, and `wait` returns 0 after it stops.
   - Old clients treat unknown states as not running. New enum values only append.

2. **The daemon owns the main process; Attach streams it.** A new bidirectional streaming RPC, `Attach`, carries a `start` message (mode exec or main process, command, tty, env, workdir, user, replay, follow), then `stdin`, `resize` and `signal` messages from the client and `data` and `exit` messages from the daemon.
   - The manager keeps the main process's stdin pipe open while no client is attached. Only an `stdin` EOF closes it. A client disconnect detaches and the process keeps running.
   - Output with no client attached goes to a bounded per-sandbox ring buffer (1 MiB). A later Attach replays it. `logs -f` is Attach in read-only replay mode. The buffer is memory only, so it is empty after a daemon restart.
   - A signal exit maps to `128 + signal`. `-t` without a terminal on stdin fails with Docker's message.
   - The guest agent speaks the same model: `Resize` and `Signal` frames, a pty session, and a raw wait status in `Exited`. The empty `Kill{}` keeps meaning SIGKILL, so old clients work.
   - `user` is rejected with UNIMPLEMENTED until user switching exists. No process starts as root by accident.

3. **Native tar `cp`.** `CopyTo` and `CopyFrom` stream tar in chunks under the 2 MiB message cap. The containment checks in `extract_rootfs` move to a shared `ward-core/src/archive.rs` that serves snapshots, `CopyTo` and `CopyFrom`, each with its own policy.
   - All callers reject absolute paths, `..`, hardlink escapes and writes through symlinks.
   - Snapshot restore keeps today's behavior: it strips setuid and setgid bits, because rootfs images ship setuid binaries. `CopyTo` and `CopyFrom` reject device nodes and setuid bits.
   - Entry count (100000), total bytes (8 GiB) and path depth (64) are capped by named constants.
   - The host validates a tar that comes from the guest, because the guest is untrusted.

4. **Persisted metadata.** Each sandbox directory holds a `meta.json` with options, name, `created_at`, state, `exit_code`, the main command and its arguments, the published host ports and a token hash.
   - `env` and the command arguments are persisted. Both can carry secrets. The file is mode 0600 and the sandbox directory is 0700.
   - Writes use a temp file, `fsync` of the file, rename, and `fsync` of the directory.
   - `ListSandboxes` and an unauthenticated `GetSandbox` return only argv[0]. The full command needs the sandbox token.

5. **Hashed tokens.** The daemon stores the SHA-256 of the capability token and never the token. The token is a 122-bit random UUID, so the hash needs no salt. `authorize` hashes the presented token and compares the hashes in constant time (`subtle`, BSD-3-Clause), also after a restart. The CLI keeps its own copy in its token store.

6. **Startup reconciliation.** `creating` and `stopping` become `failed`. A directory with a missing, unreadable or invalid `meta.json` is skipped and logged, never deleted, so a downgraded daemon cannot destroy data it cannot parse. Only a directory with neither `meta.json` nor a rootfs is deleted. Stale agent sockets are removed. No VM host process outlives the daemon (ADR-021), so there is nothing to adopt.

7. **Names and references.** Names match `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, at most 64 characters, and may not consist only of `0-9a-f-`, so a name never collides with an id prefix. Resolution is exact name, exact id, then unique id prefix. The name is reserved in the same critical section that inserts the sandbox.

8. **Wire compatibility is enforced.** `ward.proto` and `ward_agent.proto` only append fields and enum values. `scripts/check-proto-compat.sh` compares both files with a base revision and fails on a removed or renumbered field or enum value. CI runs it on every pull request.

9. **Port publishing.** The host listener binds `127.0.0.1` unless told otherwise, and the chosen host port is stored in `meta.json` and reused on every start. Inbound flows go through ward-net (see spike findings).

## Spike findings

**Host-initiated TCP into the guest works in ward-net (spike for port publishing).** A host connection can reach a guest listener through the existing smoltcp `Stack` with about 43 lines in `Stack::add_inbound_flow`. It opens a tcp socket from the gateway address (10.0.2.2) with an ephemeral local port, starts the existing host I/O bridge, and inserts a `Flow` in the established state. `pump_flows` is reused unchanged. The neighbor cache learns the guest MAC through ARP by itself.
- The guest IP is not tracked by the `Stack` outside its DHCP lease table, so the forwarder must read the lease or use a fixed IP. Connections that arrive before a lease exists are refused.
- The `Stack` runs inside one task, so the forwarder needs a `StackCommand::AddInboundFlow` message and a `poll()` right after it to avoid the 50 ms poll wait.
- Egress and SSRF checks do not apply to inbound flows. They guard guest-initiated flows only. Inbound flows count against the flow table cap.
- The guest must listen on its `eth0` address, not on its loopback.
- Size: the Stack method and plumbing are 60 to 80 lines, the listener tasks 60 to 100, so the first port publishing step is about 250 to 350 lines including tests. No new TCP state machine, ARP seeding or NAT table is needed.
- The spike used smoltcp as the guest. A real Linux guest is checked in the real-VM test.

**A pty needs no new C dependency for the agent (spike for the guest pty).** `rustix` with the `pty` and `termios` features compiles for `aarch64-unknown-linux-musl`. The static agent binary size is unchanged until code uses it (1102640 against 1103088 bytes). Whether it works at runtime is checked in the real-VM pty test. The fallback is `libc::openpty`.

## Consequences

- Sandboxes keep their slot while stopped or exited, so they count toward `max_sandboxes`. `timeout_seconds` is wall time since create and removes a sandbox in any state. `ward prune` removes every stopped and exited sandbox.
- Anyone who can read a sandbox directory can read its `env`. The 0600 and 0700 modes make that the daemon's user only, the same trust as the data directory today.
- Old on-disk sandboxes without `meta.json` are not loaded. They are left in place, never deleted.
- Every SDK and ward-mcp needs an unknown-enum test, because the status enum grows.
- A slow `events` reader loses old events and is told so. There is no resume.
- The ring buffer is lost on a daemon restart, so `logs` after a restart is empty.

## Alternatives considered

- **Client spawns the main process.** Rejected: a client disconnect would kill it, and detach could not work.
- **Store the token in `meta.json`.** Rejected: a leaked file would grant control of every sandbox. The hash gives the same reload behavior.
- **Encrypt `env` at rest.** Rejected: the key would live next to the file, with no gain over file modes.
- **Shell out to `tar` for `cp`.** Rejected: the guest may lack it, and a host `tar` cannot apply the containment policy.
- **Separate starting state.** Rejected: `creating` already means the rootfs is being prepared or the VM is booting.
- **Compile the descriptor and compare with `protoc`.** Rejected: the check would need `protoc` in the job. A text scan of the two files needs only bash, git and awk.
