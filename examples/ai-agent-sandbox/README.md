# AI agent sandbox

A 5-minute walkthrough: spawn a ward sandbox, run an untrusted command in it,
collect the output, tear the sandbox down. The flow that ward exists to
serve, distilled to the smallest concrete example.

The narrative: an AI coding agent (Claude Code, Cursor, an autonomous shell
agent, anything) needs to execute code that's been generated or supplied by
an LLM. The agent doesn't trust the code; running it directly on the host
risks file overwrites, exfiltration, fork bombs, or worse. ward gives the
agent a per-task hardware-isolated microVM in <1s, kills it cleanly, and
leaves the host untouched.

## Prerequisites

- `wardd` running (`./target/release/wardd` in a separate terminal).
- `ward` CLI on `PATH`.

Both ship with the [installer](../../install.sh) or `cargo build --release`.

## End-to-end (CLI)

```sh
# 1. Create a fresh sandbox from an OCI image. Egress defaults to Deny
#    at the daemon level when no policy is supplied, so the sandbox is
#    network-isolated out of the box. The CLI prints one field per line;
#    awk picks the id off the first line.
sb=$(ward create alpine:latest | awk '/^id:/{print $2}')
echo "sandbox: $sb"

# 2. Run untrusted code in it. `ward exec` returns immediately with a pid;
#    output streams from `ward logs <id> <pid>` once the process has run
#    (or in parallel for long-running commands).
pid=$(ward exec "$sb" -- sh -c "echo 'hello from inside'; whoami; uname -a" \
        | awk '/^pid:/{print $2}')
ward logs "$sb" "$pid"

# 3. (Optional) Run a language snippet via `ward run` with the `--code`
#    string flag. Same exec + logs pattern: pid from `ward run`, then
#    `ward logs` to drain stdout / stderr / exit.
pid=$(ward run "$sb" --language python \
        --code 'import os; print(f"hi from pid {os.getpid()}")' \
        | awk '/^pid:/{print $2}')
ward logs "$sb" "$pid"

# 4. Tear it down. The microVM exits, libkrun frees its context,
#    rootfs and overlay are reaped.
ward remove "$sb"
```

The whole flow is sub-second once the OCI image is cached locally.

> **Note** The CLI today does not expose `--egress`, `--egress-allowlist`,
> or a `--json` output flag. The daemon's gRPC API (`proto/ward.proto`)
> does carry an `EgressPolicy` field; until the CLI plumbing lands,
> sandboxes get the daemon-side default (Deny). To allow specific hosts
> today, talk to the daemon directly with a gRPC client built from the
> proto, or use one of the planned SDKs.

## Wrapping ward in an AI agent

For an agent integrating ward, the contract is:

| Step | Agent action | ward call |
|---|---|---|
| Receive untrusted code from LLM | Buffer the snippet | n/a |
| Choose isolation policy | Pick image, egress, resources | n/a |
| Allocate sandbox | `ward create <image>` (default egress: Deny) | `CreateSandbox` |
| Run the snippet | `ward exec <sb> -- <cmd>` then `ward logs <sb> <pid>` | `Exec` + `StreamOutput` |
| Collect output, exit code | Parse `stdout:` / `stderr:` / `exit:` lines | n/a |
| Tear down | `ward remove <sb>` | `RemoveSandbox` |

The ward calls map 1:1 to the gRPC API at `proto/ward.proto`. Until
the [SDKs](https://github.com/igorjs/ward/issues/39) land, agents either
shell out to the CLI (the script below) or generate a gRPC client from
the proto themselves.

### Recommended defaults for "untrusted LLM output" workloads

```
egress.mode         = Deny      (daemon default when CreateSandbox omits egress)
resources.cpus      = 1
resources.memory_mb = 512
resources.timeout   = 60s
mounts              = []
volume_ids          = []
```

- `egress = Deny` because untrusted code shouldn't reach the network.
  Switch to `Allowlist` with a list of explicit hosts (e.g. `pypi.org`,
  `registry.npmjs.org`) via the gRPC API only if package install is part
  of the task.
- `timeout = 60` because most LLM-generated snippets are short-running;
  the daemon auto-removes the sandbox if it overruns, so a runaway loop
  is bounded.
- No mounts, no volumes. If the agent needs to ferry input/output in
  and out, use stdin/stdout via `ward exec`. Never bind-mount the
  agent's working directory.

## Run the bundled script

```sh
./examples/ai-agent-sandbox/run-untrusted.sh \
  "alpine:latest" \
  "echo hello; uname -a; ls /"
```

The script wraps the four-step flow above with structured logging,
so you can copy it as the basis for your agent's wrapper.

The script uses `sh -c "$command"` inside the guest as a convenience
so callers can pass shell strings. The guest is the trust boundary,
not the script; if you'd rather avoid the in-guest shell, drop the
`sh -c` wrapper and pass argv directly (`ward exec "$sb" -- prog arg1 arg2`).

## What ward gives you that Docker doesn't

- **Hardware isolation by default.** A bug in a sandbox kernel can't
  reach the host kernel. Docker shares the host kernel.
- **Sub-second boot.** libkrun's minimal kernel + initramfs + virtio
  devices boot in under a second cold. Cached images: <100ms (anecdotal;
  no benchmark suite yet).
- **Egress allowlist as a first-class field.** Not a daemon-wide
  policy or an iptables rule you wrote; a `CreateSandboxRequest`
  field with a deny-by-default semantic.
- **No daemon-as-root requirement.** wardd runs as the invoking
  user; sandboxes inherit that boundary. Docker historically
  required root or rootless setup with caveats.

## Comparison vs cloud sandboxes (E2B / Daytona / Cloudflare Sandbox)

| Concern                 | E2B / Daytona               | ward                                |
|-------------------------|-----------------------------|-------------------------------------|
| Where the workload runs | Cloud VM in vendor's account| Local microVM on your host          |
| Data residency          | Vendor processes it         | Never leaves the host               |
| Cost model              | Per-minute or per-call      | Free; bound by your host's resources|
| Egress controls         | Vendor-managed              | You configure per-sandbox           |
| Cold start              | ~1-5 seconds                | <1s (cached image)                  |
| Internet required       | Yes                         | No                                  |

Pick ward for: local development, air-gapped environments, latency-sensitive
agents, privacy-sensitive workloads. Pick the cloud option for: workloads
that need to scale beyond your host's capacity, or that genuinely need to
run inside the vendor's network for downstream service access.

## Next steps

- [SECURITY.md](../../SECURITY.md) describes what ward isolates and what it doesn't.
- [docs/SPEC.md](../../docs/SPEC.md) is the full protocol + ADR index.
- [proto/ward.proto](../../proto/ward.proto) is the gRPC API surface.
- [ADR-008](../../docs/adr/008-egress-control.md) explains how egress filtering works.
- Issues [#39](https://github.com/igorjs/ward/issues/39),
  [#40](https://github.com/igorjs/ward/issues/40),
  [#41](https://github.com/igorjs/ward/issues/41),
  [#42](https://github.com/igorjs/ward/issues/42)
  track the planned Python / TS / Go / Rust SDKs.
