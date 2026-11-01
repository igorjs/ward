# Deploying wardd

This guide covers running `wardd` on a host you look after: where it keeps files, how to configure it, how to supervise it, and what it does and doesn't protect against. For a personal install at login, [`service.md`](service.md) is shorter. For how the pieces fit, see [`architecture.md`](architecture.md).

## Requirements

- Linux 5.10+ with KVM, or macOS 12+ on Apple Silicon. See [`platforms.md`](platforms.md).
- A `wardd` built with the `krunvm` feature. The stub build boots no VMs.
- On Linux, the daemon's user needs read and write access to `/dev/kvm`. Add it to the `kvm` group.
- `mkfs.ext4` on the host, if you create volumes.

## Files and sockets

| Item | Default | Override |
| --- | --- | --- |
| Socket, macOS | `~/.ward/ward.sock` | `WARD_SOCKET` |
| Socket, Linux | `$XDG_RUNTIME_DIR/ward/ward.sock`, else `/tmp/ward-$USER/ward.sock` | `WARD_SOCKET` |
| Data directory | `~/.ward/data` | `WARD_DATA_DIR` |

The data directory holds four subdirectories: `images`, `sandboxes`, `snapshots` and `volumes`. Each sandbox keeps a `meta.json` under `sandboxes`. The daemon creates every directory with mode `0700`. It refuses to use a path that's a symlink. It sets its umask to `0077` before binding, and sets the socket to mode `0600`. Only the owning user connects.

The daemon needs `HOME` only when you leave `WARD_DATA_DIR` unset. A system service has no home, so set both paths yourself.

The `ward` CLI reads `WARD_SOCKET` too, or takes `--socket`. Point it at the same path as the daemon.

Plan disk for the data directory. Images, sandbox root filesystems and snapshots all live there.

## Environment variables

`wardd` reads these once at startup. A change needs a restart.

| Name | Default | Meaning |
| --- | --- | --- |
| `WARD_SOCKET` | see above | Unix socket path. The parent directory is created with mode `0700`. |
| `WARD_DATA_DIR` | `~/.ward/data` | Root of the data tree. See the subdirectories above. |
| `WARD_LOG_LEVEL` | `info` | A `tracing` filter. Takes a level (`debug`) or per-target rules (`ward=trace`). |
| `WARD_MAX_SANDBOXES` | `256` | Most sandboxes at once. Stopped ones count. Create fails past it. Also sets the global RPC cap, at twice this value. |
| `WARD_MAX_VOLUMES` | `256` | Most volumes. |
| `WARD_MAX_CACHED_IMAGES` | `64` | Most cached OCI images. A pull evicts unprotected entries when the cache is full. Committed images and images a sandbox runs are kept. |
| `WARD_ALLOW_HOST_MOUNTS` | off | Set to `1` or `true` to allow bind mounts from any host path. Any other value keeps the default, which allows only `/home/`, `/tmp/` and `/var/lib/ward/`. |
| `WARD_METRICS_ADDR` | unset | A `host:port` for a Prometheus `/metrics` endpoint, such as `127.0.0.1:9100`. An unparseable value logs a warning and leaves metrics off. |
| `WARD_NETWORK_BACKEND` | `smoltcp` | `smoltcp` or `none`. `none` gives sandboxes no network and rejects `allowlist` egress. An invalid value stops the daemon at startup. |
| `WARD_SHUTDOWN_TIMEOUT_SECS` | `30` | Time allowed to drain RPCs and remove every sandbox after SIGTERM or SIGINT. The daemon exits with status 1 if it runs out. Values below 1 become 1. |
| `WARD_EXITED_TTL_SECS` | off | Remove a sandbox this many seconds after it ends. A sandbox's own `--exited-ttl` wins. |
| `WARD_DEFAULT_MEMORY_MB` | `0` (none) | Memory limit for a sandbox that asks for none. |
| `WARD_DEFAULT_CPUS` | `0` (none) | CPU count for a sandbox that asks for none. |
| `WARD_DEFAULT_PIDS_LIMIT` | `0` (none) | Process cap for a sandbox that asks for none. |
| `WARD_REGISTRY_ALLOWLIST` | unset | Comma-separated registries, such as `docker.io,ghcr.io`. Unset or blank allows any registry. A blank value logs a startup warning. |

Numeric variables that don't parse fall back to their default, except where the table says otherwise. A zero, or a value that doesn't parse, leaves the three `WARD_DEFAULT_*` limits and `WARD_EXITED_TTL_SECS` off.

`WARD_OCI_OFFLINE`, `WARD_OCI_OFFLINE_CONFIG`, `WARD_RUNTIME_DIR`, `WARD_KVM_TESTS` and `WARD_LIBKRUN_TARBALL` exist for tests and builds. Don't set them in production.

## Resource caps

Like Docker, a sandbox has no memory limit, CPU limit or pids limit unless someone sets one. The daemon sets no limits by itself. Set the `WARD_DEFAULT_*` variables to opt in. They apply to any field a request leaves at 0. `-m`, `--cpus` and `--pids-limit` on the request override them.

A request is rejected when it asks for more than these ceilings:

| Field | Ceiling |
| --- | --- |
| CPUs | 64 |
| Memory | 65536 MiB |
| Pids | 65536 |
| Timeout | 2592000 seconds (30 days) |

`ward inspect` shows the effective values in `HostConfig`. The VM backend enforces all three. The pids cap is shared by the main process and every exec. It needs a guest kernel with cgroup v2 and the pids controller. Without them, the agent logs that the limit isn't enforced and the sandbox runs unlimited.

On a shared host, set all three defaults and `WARD_MAX_SANDBOXES` so that the worst case fits in RAM. For example, 32 sandboxes at 2048 MiB need 64 GiB.

## Linux: systemd

The shipped user unit is [`packaging/systemd/wardd.service`](../packaging/systemd/wardd.service). It sets `NoNewPrivileges`, `RestrictSUIDSGID` and `LockPersonality`, and nothing stricter. `wardd` boots VMs through libkrun, which needs `/dev/kvm`, anonymous executable memory, and a re-exec of its own binary for each VM. So `PrivateDevices`, `MemoryDenyWriteExecute` and syscall filters stay off.

For a system service, run a dedicated user and tighten what's left:

```ini
# /etc/systemd/system/wardd.service
[Unit]
Description=Ward sandbox daemon
Documentation=https://github.com/igorjs/ward
After=network.target

[Service]
Type=simple
User=ward
Group=ward
# Gives access to /dev/kvm.
SupplementaryGroups=kvm
ExecStart=/usr/local/bin/wardd
Restart=on-failure
RestartSec=5
TimeoutStopSec=40

Environment=WARD_LOG_LEVEL=info
Environment=WARD_DATA_DIR=/var/lib/ward
Environment=WARD_SOCKET=/run/ward/ward.sock
Environment=WARD_MAX_SANDBOXES=32
Environment=WARD_DEFAULT_MEMORY_MB=2048
Environment=WARD_DEFAULT_CPUS=2
Environment=WARD_DEFAULT_PIDS_LIMIT=512

StateDirectory=ward
StateDirectoryMode=0700
RuntimeDirectory=ward
RuntimeDirectoryMode=0700

NoNewPrivileges=yes
RestrictSUIDSGID=yes
LockPersonality=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/ward /run/ward
ProtectHome=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictNamespaces=yes
RestrictRealtime=yes
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
```

Notes:

- `/dev/kvm` is not hidden because `PrivateDevices` is off. The `kvm` group controls access.
- `StateDirectory=ward` creates `/var/lib/ward`, one of the three default bind-mount roots.
- `ProtectSystem=strict` makes the file system read-only except for `ReadWritePaths`. If you bind-mount host paths into sandboxes under `WARD_ALLOW_HOST_MOUNTS`, add them to `ReadWritePaths` when a sandbox must write to them.
- `ProtectHome=yes` hides `/home`, so bind mounts from there fail. Use `read-only` or drop the line if you need them.
- `TimeoutStopSec` should exceed `WARD_SHUTDOWN_TIMEOUT_SECS`, so systemd waits for the drain. The shipped unit uses 40 against the 30 second default.
- Run `systemd-analyze security wardd` to review the unit, and start a test sandbox on your kernel before you rely on it.
- The daemon forces the socket to `0600`, so only the `ward` user connects. Run the CLI as that user, or through `sudo -u ward`.

For a per-user install, see [`service.md`](service.md): `systemctl --user enable --now wardd`, then `loginctl enable-linger $USER` to survive logout.

## macOS: launchd

The shipped template is [`packaging/launchd/dev.ward.wardd.plist`](../packaging/launchd/dev.ward.wardd.plist). launchd doesn't expand `~` or `$HOME`, so fill in the two placeholders as [`service.md`](service.md) shows. The result looks like this:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.ward.wardd</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/you/.ward/bin/wardd</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>WorkingDirectory</key>
  <string>/Users/you</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key>
    <string>/Users/you</string>
    <key>WARD_LOG_LEVEL</key>
    <string>info</string>
  </dict>
  <key>StandardOutPath</key>
  <string>/Users/you/Library/Logs/ward/wardd.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/you/Library/Logs/ward/wardd.log</string>
</dict>
</plist>
```

Add other `WARD_*` variables to `EnvironmentVariables`. The daemon must be code-signed with the Hypervisor entitlement. Release binaries are. A local build needs the signing step in the README.

### Sleep and Power Nap

A LaunchAgent runs in your login session. It stops when you log out, and it doesn't run before login. For a headless Mac, use a login item with auto-login, or a LaunchDaemon with a dedicated user, which needs its own `UserName` key and a data directory that user owns.

When the Mac sleeps, the daemon and its VMs stop with it. Power Nap lets macOS run some background work during sleep, but you shouldn't rely on it to keep a VM running or to answer requests. Plan on the sandboxes being paused while the lid is closed or the machine is idle-asleep. The effects:

- A client that waits on a sandbox sees a long gap after wake.
- Timeouts set with `--timeout` are your protection against a sandbox left over after a stall. Pick them with sleep in mind.
- To keep the Mac awake while the daemon runs, wrap `ProgramArguments` in `/usr/bin/caffeinate -i`, or set power policy with `pmset`. Both trade battery for availability.

Use `launchctl bootout "gui/$(id -u)/dev.ward.wardd"` to stop it.

## Logs

`wardd` logs through `tracing` to standard output, in the default human-readable format. It opens no log file and doesn't rotate anything. Whoever starts the daemon decides where the output goes:

| Supervisor | Destination | Rotation |
| --- | --- | --- |
| systemd | journald. Read with `journalctl -u wardd` (add `--user` for a user unit). | journald's own limits in `journald.conf`. |
| launchd | The files in `StandardOutPath` and `StandardErrorPath`. | None built in. See below. |
| Homebrew services | `$(brew --prefix)/var/log/ward.log`. | None built in. |
| Your own wrapper | Wherever you redirect it. | Yours. |

Set the level with `WARD_LOG_LEVEL`. `debug` is verbose. Use per-target filters to raise one area.

The daemon doesn't reopen its output after a rotation. A tool that renames the file leaves `wardd` writing into the renamed copy. Use a copy-and-truncate rotation, or restart the daemon after you rotate. For `logrotate` on a host where you redirect to a file:

```
/var/log/ward/wardd.log {
    weekly
    rotate 8
    compress
    missingok
    copytruncate
}
```

On macOS, `newsyslog` rotates by renaming. Pair it with a `launchctl kickstart -k "gui/$(id -u)/dev.ward.wardd"` so the daemon opens the new file. A restart removes every sandbox (see Shutdown), so schedule it with care.

Secrets don't appear in logs. See [`docker-differences.md`](docker-differences.md).

### Metrics

Set `WARD_METRICS_ADDR=127.0.0.1:9100` to expose `/metrics`. The endpoint has no authentication, so bind it to loopback, or put it behind a firewall or reverse proxy. Broker counters include `wardd_broker_publish_total`, `wardd_broker_deliver_total` and `wardd_broker_drop_total`. Alert on the drop counter rising.

## Shutdown and restart

On SIGTERM or SIGINT the daemon stops taking RPCs, drains the ones in flight, and removes every sandbox. If that takes longer than `WARD_SHUTDOWN_TIMEOUT_SECS`, it removes the socket and exits with status 1.

A clean shutdown removes every sandbox, so a restart starts empty. Plan for that when you roll the daemon: sandboxes don't carry over. After a crash or SIGKILL the records stay on disk. The next start reloads each one as stopped, because no VM outlives the daemon. Start them again with `ward start`. A sandbox created with `--secret` can't be started again, because its secrets aren't stored.

## Multi-tenant security

`wardd` is built for one trust domain per daemon. These notes say what separates callers inside it, and what doesn't.

### Who can connect

The socket is mode `0600` and its directory `0700`. Any process running as the daemon's user can call every RPC that needs no token. Anyone who can read the data directory can read sandbox contents. There's no user authentication. Don't expose the socket to untrusted users, over a network forward or a shared group. [ADR-013](adr/013-multi-tenant-auth.md) describes the intended multi-tenant model, which isn't in the code yet.

### Capability tokens

`create` returns a random token once. The daemon stores only its SHA-256 hash and compares it in constant time. Calls that act on a sandbox (exec, attach, copy, stop, remove, snapshot, publish, subscribe, logs) must present it in the `x-ward-sandbox-token` header. A wrong or missing token returns permission denied. Creating a sandbox from a snapshot needs the token of the sandbox that owns the snapshot.

A token proves possession, not identity. Whoever holds it has full control of that sandbox. Treat it like a password. `get` and `list` redact it, and `ward ps` and `ward inspect` show only `argv[0]` of the command unless you hold the token.

Some calls need no token: `ListSandboxes` (token redacted), `GetHealth`, `GetInfo`, `Events`, `ListImages`, `PullImage` and the volume RPCs.

### Unauthenticated events

`Events` needs no sandbox token. Anyone who reaches the socket sees every sandbox's lifecycle events: sandbox ids and image names. The log holds the last 1024 events and stays in memory. This is a known gap, noted in [`docker-differences.md`](docker-differences.md). Until it's fixed, treat anyone with socket access as an operator.

### Sandbox limits

- Each sandbox is a microVM with its own kernel. A guest bug doesn't touch host namespaces.
- Resource limits are off by default. Without `WARD_DEFAULT_*` values, one sandbox uses all the host memory it asks for. Set the defaults on a shared host.
- `WARD_MAX_SANDBOXES` and `WARD_MAX_VOLUMES` stop resource exhaustion by count.
- Use `--timeout` or `--exited-ttl` (or `WARD_EXITED_TTL_SECS`) so abandoned sandboxes don't pile up.
- The pids cap and the memory cap rely on guest support. Check `ward inspect` and the agent log.
- Egress defaults to `deny`. Use `allowlist` for sandboxes that need some access. `open` lets a sandbox reach the host's network, so treat it as trusted code only. Inbound published ports bypass egress policy and bind `127.0.0.1` unless you set a host address.
- Copies are checked as they pass. A tar with absolute paths, `..`, hardlinks out of the root, or special files is refused.

### Host mounts gate

Bind mounts accept only paths under `/home/`, `/tmp/` and `/var/lib/ward/`. `WARD_ALLOW_HOST_MOUNTS=1` removes that limit. A sandbox with a writable bind mount can change host files as the daemon's user. The setting is read once at startup, so a guest that learns the environment can't widen it later. Leave it off unless the host's callers are all trusted. Prefer read-only mounts.

### Comms group isolation

Sandboxes can't talk to each other by default. Cross-sandbox messages go through the broker only when both sandboxes are in `group` mode with the same non-empty group name. Treat the group name as the isolation boundary: give each tenant a distinct, unguessable group, and don't put two tenants in one group. The broker is in-process and per daemon. The audit log (`GetCommunicationLog`) shows allowed and denied attempts, 256 entries per sandbox, and needs the sandbox token.

Delivery is lossy and payloads are capped at 1 MiB, so don't use the broker for anything that must arrive.

### Registries and images

Set `WARD_REGISTRY_ALLOWLIST` to the registries you trust. Use `--pull=pinned` with digest references where you want reproducible images. Signature verification isn't implemented yet.
