# Where Ward differs from Docker

Ward copies Docker's command line where it can. This page lists the places it does not, and why. Each item was checked against the CLI and daemon code. Gaps that are bugs, not choices, link to their issue.

## Isolation

Ward runs each sandbox in a microVM, not in Linux namespaces. The guest has its own kernel. Docker flags that change host privileges have no meaning there. `ward run` and `ward create` refuse `--privileged`, `--cap-add`, `--cap-drop`, `--security-opt`, `--device`, `--network` (and `--net`), `--ipc`, `--pid`, `--uts`, `--userns` and `--cgroupns` with a one line reason, and reject any other flag not listed in their `--help`. `--privileged=false` is accepted, as it changes nothing. Network access is set with `--egress` instead.

## Build

There is no `ward build` and no Dockerfile support yet. Ward runs images from a registry. `ward commit` saves a sandbox as a local image. Design work is in [#202](https://github.com/igorjs/ward/issues/202) and [#203](https://github.com/igorjs/ward/issues/203).

## Resources

`--memory` without a suffix means bytes, as in Docker. Older Ward read a bare number as MiB. The value is rounded down to whole MiB, and anything under 1 MiB fails. `0` uses the daemon default.

## Create options

These flags of `ward run` and `ward create` follow Docker unless noted.

- `--env-file FILE` reads `KEY=VALUE` lines. Blank lines and lines starting with `#` are skipped, and a bare `KEY` takes the host value or is dropped when the host has none. `-e` overrides the files, and a later file overrides an earlier one. Values are not unquoted or expanded.
- `-l/--label KEY[=VALUE]` and `--label-file FILE` set labels, shown in `inspect` as `Config.Labels`. `-l` overrides the file. A sandbox takes at most 128 labels, with keys up to 256 bytes and values up to 4096. Labels also appear as attributes of its `ward events`, so `events --filter label=KEY[=VALUE]` works.
- `--cidfile FILE` writes the sandbox ID without a newline and fails if FILE exists. The file is removed when creation fails.
- `--replace` is Podman's: it removes an existing sandbox with the same `--name`, running or not, before creating the new one. Without `--name` it does nothing. Podman stops the old container with its own stop timeout; Ward removes it as `rm -f` does.
- `--platform linux/ARCH` is accepted only when it names this host's architecture (`linux`, `linux/amd64` or `linux/arm64`, with the `v8` variant for arm64). Ward cannot emulate another architecture, so any other value is an error.
- `--hostname NAME` sets the guest host name, in the kernel and in `/etc/hostname`. Docker's `-h` short form is not available, because `-h` is help in Ward. Without it the host name is the first 12 characters of the sandbox ID, as in Docker. `inspect` shows it as `Config.Hostname`. Ward does not add the name to `/etc/hosts`, so the guest cannot resolve it through that file. A name takes up to 64 letters, digits, `-`, `.` and `_`, and starts with a letter or digit.
- `--stop-signal SIGNAL` (a name such as `SIGINT` or a number) replaces SIGTERM as the signal that asks the main process to end. `--stop-timeout SECONDS` replaces the 10 second wait before SIGKILL; `-1` waits forever and `0` kills at once. Both apply to `stop` and `restart`, and `stop -t` or `restart -t` overrides the timeout. `inspect` shows `Config.StopSignal` and `Config.StopTimeout` once they are set, as Docker does. `rm -f` does not send the stop signal: it removes the sandbox at once, as Ward always has.
- `--restart no|on-failure[:N]|always|unless-stopped` follows Docker. The daemon starts the sandbox again when its main process ends: `always` and `unless-stopped` after any exit, `on-failure` only after a non-zero exit and at most N times (0 or no N means no limit). The wait between restarts starts at 100 ms, doubles each time up to 1 minute, and starts over after a run of 10 seconds or more. `stop` and `rm` end the restarts, and so does `kill` with SIGKILL or the stop signal; any other kill signal does not. A start or `restart` by hand clears that and resets `RestartCount`, which `inspect` shows at its top level, with the policy in `HostConfig.RestartPolicy`. Differences: a sandbox waiting for its next restart shows `exited`, not `restarting`; the daemon restarts `always` and `unless-stopped` sandboxes when it starts (and `unless-stopped` only if not stopped by hand) but a clean `wardd` shutdown removes every sandbox first, so this applies after a crash; `--restart` other than `no` cannot be combined with `--secret`, because a fresh VM has no secrets to deliver; `--rm` with `--restart` other than `no` is refused, as in Docker.
- `--init` is accepted and does nothing.

## Ports

- `-p` binds `127.0.0.1` when you give no `HOST_IP`. Docker binds `0.0.0.0`. Ward keeps ports private unless you ask.
- A published port stays open while the sandbox is exited. It closes on `stop` or `rm`.
- Only TCP is supported.

## Network egress

Ward has three modes: `deny` (default), `allowlist` and `open`. In `allowlist` mode the sandbox's DNS resolver answers REFUSED for any name not on the list. The host resolver is an outbound path, so it does not see those names.

`ward egress suggest SANDBOX` has no Docker equivalent. It prints an `--allow-domain` flag for each host the sandbox was refused, and lists blocked IP addresses on stderr. The record is kept after the sandbox exits, until `rm`. 

`ward egress suggest SANDBOX --apply` adds the suggested names to the sandbox's allowlist. It takes effect at once, without a restart, and the names are kept across daemon restarts. IP addresses are skipped. It works only on a sandbox created with `--egress allowlist`: a `deny` sandbox has no network device, so it must be recreated, and an `open` sandbox has nothing to add. An allowlist holds at most 256 names.

A `deny` sandbox without published ports records nothing for `suggest`. The guest has no network card, so there is no outbound attempt for Ward to see. With published ports the stack runs and refusals are recorded.

## Secrets in process lists

- `ward ps` shows only `argv[0]` in the COMMAND column. Arguments often hold secrets.
- `ward inspect` shows the full command only when you hold the sandbox token. Without it, you see `argv[0]` only.

## Attach

Each attach session is separate. A client that disconnects only detaches and leaves the main process running. In raw terminal mode Ctrl-C goes to the guest as a byte, as in Docker. Outside raw mode the CLI passes SIGINT and SIGTERM to the guest process.

## Commands with a smaller option set

- `ward ps` supports `-a`, `-q`, `-n`, `-l`, `--no-trunc`, `--filter` and a Go `--format` over `.ID .Image .Command .CreatedAt .RunningFor .State .Status .Reason .Names .Labels`, plus `{{.Label "key"}}`. `--filter` takes `name`, `id`, `status` and `label=KEY[=VALUE]` (every label filter must match); `name` is a substring match, not a regular expression, and `label!=` is not supported. `.Ports`, `.Mounts`, `.Networks`, `.Size` and `.LocalVolumes` do not exist yet. COMMAND is `argv[0]` only. `State` also has `stopped` and `failed`, which Docker lacks.
- `ward inspect` takes `--format` as a Go template over the JSON it prints. Only `{{.Path}}` and `{{json .Path}}` work: no `range`, `if`, `index` or functions. An array field prints as JSON, not as Go's `[a b]`. `--type container` is accepted and `--type image` is refused, because Ward cannot inspect images yet.
- `ward container` and `ward image` are spellings of the top-level commands. `container` has `ls`, `ps`, `create`, `run`, `start`, `stop`, `restart`, `rm`, `kill`, `logs`, `inspect`, `exec`, `cp`, `top`, `stats`, `wait`, `diff`, `commit`, `prune`, `port` and `rename`; `image` has `ls` and `pull`. The other Docker subcommands are not there yet.
- `ward version` prints the client and daemon versions. Unlike Docker it has no API version, Go version, git commit or build time. Without a daemon it prints the client half and exits 1. `--format` fields are `.Client.Version` and `.Server.Version`, `.Server.Os`, `.Server.Arch` and `.Server.Backend`.
- `ward system info` is `ward info`, and `ward system prune` is `ward prune`: both remove only stopped and exited sandboxes, never images, volumes or networks, and take no `-a`, `-f` or `--filter`. `ward system df` lists the sandbox count (with how many run) and the image cache size; it has no per-image breakdown and no `-v`.
- `ward rename` prints nothing on success, as Docker does. It works on a running sandbox. Docker's `rename` event carries the old name; Ward's does not.
- `ward stats` NET I/O counts the Ethernet bytes that cross the host network stack since the sandbox last started, so it includes headers. BLOCK I/O comes from the sandbox cgroup's `io.stat` (device bytes) when that has data. On virtiofs or tmpfs, or without an io controller, no block device is touched, so it instead reports the bytes the sandbox processes passed to read and write calls (`rchar` and `wchar`), sampled twice a second and kept for processes that exited. That count includes pipes and sockets, and misses a process that starts and exits between two samples.
- `ward images -a` adds nothing, and no image is dangling. Ward keeps no intermediate images.
- `ward commit --pause` is accepted, but the copy is live. The commit keeps hard links, sparse holes, modification times (whole seconds), `user.*` extended attributes, file capabilities, FIFOs and device nodes. Sockets are left out, as in Docker. Creating a device node needs privilege, so an unprivileged daemon leaves it out, and macOS hosts leave out FIFOs and device nodes. Extended attributes in other namespaces (SELinux labels, ACLs) are not kept, and setting a file capability needs `CAP_SETFCAP`. Where the daemon cannot set an attribute, the commit still succeeds without it.
- `ward cp` ignores `-a` and `-q`. A copy into a sandbox is checked by the guest while it unpacks, so a tar that fails midway leaves the entries already written, as in Docker. A guest that cannot finish a copy reports an internal error with its own reason.
- `ward pull` and image storage have gaps: one cache entry per digest, no re-resolve of tags, no eviction on create. See [#252](https://github.com/igorjs/ward/issues/252).

## Image management

- `ward rmi` takes a tag, an image ID or a unique ID prefix, and `--no-prune` is not accepted. An image a running sandbox uses is never deleted, even with `-f`. With `-f` it deletes an image that stopped sandboxes use; the sandbox keeps its own copy of the files. Several tags given by ID need `-f`, as in Docker.
- `ward tag` moves a tag that another image holds. A name with no tag means `:latest`.
- `ward history` reads the build steps from the image config, so an image cached by an older Ward shows none. The IMAGE column shows the ID on the top row and `<missing>` on the others. A committed image adds one step whose SIZE is the whole filesystem, not the change, because Ward keeps no layers.
- `ward save` writes an OCI image layout that also carries Docker's `manifest.json`. Ward keeps unpacked files, not registry layers, so each image is one flattened layer. File owners are not kept, so the layer records root, and directory and symlink times are zero. Hard links become copies, and sockets, pipes and device nodes are left out. Saving an image that was loaded gives the same archive. A tag given to `save` saves that tag only; an ID saves every tag of the image. Writing to a terminal is refused.
- `ward load` reads `ward save`, Docker and OCI layout archives, gzip or plain layers. An image index with several platforms is not supported. The loaded image gets the digest of the archive's manifest, or, for a Docker archive with no manifest, of its config. Because the layers are flattened, a pulled image saved and loaded gets a new digest, so a `@sha256` pin from the registry no longer matches. Layer and manifest digests are checked. A loaded image is never evicted from the cache, since nothing can pull it back. Archives up to 2 GiB are accepted.

- `ward export` reads the sandbox's files from the host, so it works in any state without a running guest. Owners are written as root, directory and symlink times are zero, hard links become copies, and sockets, pipes and device nodes are left out. Files Ward wrote at boot are left out.
- `ward import` takes a file or `-`, plain or gzip. A URL is not supported, and `--platform` is not accepted. `-c` takes CMD, ENTRYPOINT, ENV, WORKDIR and USER. The image is kept like a committed one: it is never evicted.
- `ward pull --platform` accepts only `linux/` plus this machine's architecture, because a microVM runs the host's architecture. `--all-tags` is refused. `ward search` is not available; Docker Hub's search API has no OCI equivalent.
- `ward login` and `ward logout` are not available yet. Pulls are anonymous, so private registries cannot be reached.

## Events

`ward events` needs no sandbox token. The daemon keeps only the last 1024 events in memory. Nothing survives a daemon restart.

## Ward additions

- `--pull=pinned`: like `missing`, but the image reference must carry an `@sha256` digest.
- `--exited-ttl`: removes a sandbox a set time after it ends.
- Default limits: like Docker, a sandbox has no memory, CPU or pids limit unless you set one. The operator can opt in to daemon defaults with `WARD_DEFAULT_MEMORY_MB`, `WARD_DEFAULT_CPUS` and `WARD_DEFAULT_PIDS_LIMIT`. They apply to a field a request leaves at 0, and `-m`, `--cpus` and `--pids-limit` override them. Unset or 0 means no default. `ward inspect` shows the effective values in `HostConfig`. The VM backend enforces memory, CPUs and pids. The pids limit is one cap for the whole sandbox, shared by the main process and every exec, like Docker's per-container cap; a fork past it fails with EAGAIN inside the guest. It needs a guest kernel with cgroup v2 and the pids controller. Without them the agent logs that the limit is not enforced and the sandbox runs unlimited; there is no RLIMIT_NPROC fallback, because it is per user and root in the guest bypasses it.
- `State.Reason` in `inspect` and `Reason` in `ps --format json`: why the sandbox ended (`exited`, `signal`, `oom`, `timeout` or `egress-policy`). `oom` sets `State.OOMKilled` and exits 137, as in Docker. It is recorded when the main process dies by SIGKILL and the guest kernel's OOM killer ended a process during its run, counted from the sandbox cgroup's `memory.events` (or guest wide `/proc/vmstat` without the memory controller). A sandbox that hits its `--timeout` is removed outright, so no state keeps `timeout`; the `die` event in `ward events` carries `reason=timeout` instead. `egress-policy` is never recorded: the egress policy blocks connections but does not end a sandbox.
- `ward run-code`: runs a language snippet in a sandbox. Docker has no equivalent. The daemon does not implement it yet, so use `ward exec` for now.
- `--secret [NAME=]FILE` on `run` and `exec`: puts the file in the guest at `/run/secrets/NAME` as a read-only file (mode 0400) on a tmpfs, owned by the user the command runs as (root if none). NAME defaults to the base name of FILE. The flag can repeat, up to 16 files of 64 KiB each. The CLI reads FILE, and the daemon holds the content in memory only. It never appears in `inspect`, `ps`, events, daemon logs, error messages or the sandbox record on disk. `-e` values still show in `inspect`, as in Docker. `create` refuses `--secret`, because Ward writes secrets when a process starts. A sandbox created with `--secret` cannot be started or restarted, because a fresh VM has an empty `/run/secrets`: `start` and `restart` fail, so create it again with `ward run`. Only a flag that secrets were given is stored, never their content. Files delivered by `exec --secret` stay until the sandbox stops, and an exec replaces an existing file of the same name.
- Sandbox pub/sub (`publish`, `subscribe`), snapshots and `--comms-mode`.
