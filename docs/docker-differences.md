# Where Ward differs from Docker

Ward copies Docker's command line where it can. This page lists the places it does not, and why. Each item was checked against the CLI and daemon code. Gaps that are bugs, not choices, link to their issue.

## Isolation

Ward runs each sandbox in a microVM, not in Linux namespaces. The guest has its own kernel. Docker flags that change host privileges have no meaning there. `ward run` and `ward create` reject any flag not listed in their `--help`, including `--privileged`, `--cap-add`, `--device` and `--network`. Network access is set with `--egress` instead.

## Build

There is no `ward build` and no Dockerfile support yet. Ward runs images from a registry. `ward commit` saves a sandbox as a local image. Design work is in [#202](https://github.com/igorjs/ward/issues/202) and [#203](https://github.com/igorjs/ward/issues/203).

## Resources

`--memory` without a suffix means bytes, as in Docker. Older Ward read a bare number as MiB. The value is rounded down to whole MiB, and anything under 1 MiB fails. `0` uses the daemon default.

## Ports

- `-p` binds `127.0.0.1` when you give no `HOST_IP`. Docker binds `0.0.0.0`. Ward keeps ports private unless you ask.
- A published port stays open while the sandbox is exited. It closes on `stop` or `rm`.
- Only TCP is supported.

## Network egress

Ward has three modes: `deny` (default), `allowlist` and `open`. In `allowlist` mode the sandbox's DNS resolver answers REFUSED for any name not on the list. The host resolver is an outbound path, so it does not see those names.

`ward egress suggest SANDBOX` has no Docker equivalent. It prints an `--allow-domain` flag for each host the sandbox was refused, and lists blocked IP addresses on stderr. The record is kept after the sandbox exits, until `rm`. `--apply` is not supported yet because the allowlist is fixed at create time.

## Secrets in process lists

- `ward ps` shows only `argv[0]` in the COMMAND column. Arguments often hold secrets.
- `ward inspect` shows the full command only when you hold the sandbox token. Without it, you see `argv[0]` only.

## Attach

Each attach session is separate. A client that disconnects only detaches and leaves the main process running. In raw terminal mode Ctrl-C goes to the guest as a byte, as in Docker. Outside raw mode the CLI passes SIGINT and SIGTERM to the guest process.

## Commands with a smaller option set

- `ward ps` supports `-a`, `-q` and `--format json`. Other Go templates and `--filter` are not supported.
- `ward stats` does not measure network or block I/O. NET I/O and BLOCK I/O print `--`.
- `ward images -a` adds nothing, and no image is dangling. Ward keeps no intermediate images.
- `ward commit --pause` is accepted, but the copy is live. The commit does not keep hard links, extended attributes, sparse holes or modification times, and it skips device nodes and pipes. See [#256](https://github.com/igorjs/ward/issues/256).
- `ward cp` ignores `-a` and `-q`. Some error messages and one edge case differ. See [#245](https://github.com/igorjs/ward/issues/245).
- `ward pull` and image storage have gaps: one cache entry per digest, no re-resolve of tags, no eviction on create. See [#252](https://github.com/igorjs/ward/issues/252).

## Events

`ward events` needs no sandbox token. The daemon keeps only the last 1024 events in memory. Nothing survives a daemon restart.

## Ward additions

- `--pull=pinned`: like `missing`, but the image reference must carry an `@sha256` digest.
- `--exited-ttl`: removes a sandbox a set time after it ends.
- Default limits: like Docker, a sandbox has no memory, CPU or pids limit unless you set one. The operator can opt in to daemon defaults with `WARD_DEFAULT_MEMORY_MB`, `WARD_DEFAULT_CPUS` and `WARD_DEFAULT_PIDS_LIMIT`. They apply to a field a request leaves at 0, and `-m`, `--cpus` and `--pids-limit` override them. Unset or 0 means no default. `ward inspect` shows the effective values in `HostConfig`. The VM backend enforces memory and CPUs. `--pids-limit` is stored and shown but not enforced yet.
- `State.Reason` in `inspect` and `Reason` in `ps --format json`: why the sandbox ended (`exited`, `signal`, `oom`, `timeout` or `egress-policy`).
- `ward run-code`: runs a language snippet in a sandbox. Docker has no equivalent. The daemon does not implement it yet, so use `ward exec` for now.
- Sandbox pub/sub (`publish`, `subscribe`), snapshots and `--comms-mode`.
