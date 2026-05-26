# AI agent sandbox

A 5-minute walkthrough: spawn a ward sandbox, run an untrusted command in it,
collect the output, tear the sandbox down. The flow that ward exists to
serve, distilled to the smallest concrete example.

The narrative: an AI coding agent (Claude Code, Cursor, an autonomous shell
agent, anything) needs to execute code that's been generated or supplied by
an LLM. The agent doesn't trust the code — running it directly on the host
risks file overwrites, exfiltration, fork bombs, or worse. ward gives the
agent a per-task hardware-isolated microVM in <1s, kills it cleanly, and
leaves the host untouched.

## Prerequisites

- `wardd` running (`./target/release/wardd` in a separate terminal).
- `ward` CLI on `PATH`.

Both ship with the [installer](../../install.sh) or `cargo build --release`.

## End-to-end (CLI)

```sh
# 1. Create a fresh sandbox from an OCI image.
sb=$(ward create alpine:latest --json | jq -r .id)
echo "sandbox: $sb"

# 2. Run untrusted code in it. The argv array goes straight to the guest's
#    process spawn — no shell, no string interpolation, no injection
#    surface.
ward exec "$sb" -- sh -c "echo 'hello from inside'; whoami; uname -a"

# 3. (Optional) Stream a long-running command's output.
ward run "$sb" --language python <<'PY'
import time, os
for i in range(3):
    print(f"tick {i} pid={os.getpid()}")
    time.sleep(1)
print("done")
PY

# 4. Tear it down. The microVM exits, libkrun frees its context,
#    rootfs and overlay are reaped.
ward remove "$sb"
```

The whole flow is sub-second once the OCI image is cached locally.

## Wrapping ward in an AI agent

For an agent integrating ward, the contract is:

| Step | Agent action | ward call |
|---|---|---|
| Receive untrusted code from LLM | Buffer the snippet | — |
| Choose isolation policy | Pick image, egress, resources | — |
| Allocate sandbox | `ward create <image> --egress deny` | `CreateSandbox` |
| Run the snippet | `ward exec <sb> -- <cmd>` | `Exec` + `StreamOutput` |
| Collect output, exit code | Parse JSON or stream | — |
| Tear down | `ward remove <sb>` | `RemoveSandbox` |

The four ward calls map 1:1 to the gRPC API at `proto/ward.proto`. Until
the [SDKs](https://github.com/igorjs/ward/issues/39) land, agents either
shell out to the CLI (the script below) or generate a gRPC client from
the proto themselves.

### Recommended defaults for "untrusted LLM output" workloads

```
egress.mode = Deny
resources.cpus = 1
resources.memory_mb = 512
resources.timeout_seconds = 60
mounts = []
volume_ids = []
```

- `egress = Deny` because untrusted code shouldn't reach the network.
  Switch to `Allowlist` with a list of explicit hosts (e.g. `pypi.org`,
  `registry.npmjs.org`) only if package install is part of the task.
- `timeout = 60` because most LLM-generated snippets are short-running;
  the daemon auto-removes the sandbox if it overruns, so a runaway loop
  is bounded.
- No mounts, no volumes. If the agent needs to ferry input/output in
  and out, use stdin/stdout via `ward exec` — never bind-mount the
  agent's working directory.

## Run the bundled script

```sh
./examples/ai-agent-sandbox/run-untrusted.sh \
  "alpine:latest" \
  "echo hello; uname -a; ls /"
```

The script wraps the four-step flow above with structured logging,
so you can copy it as the basis for your agent's wrapper.

## What ward gives you that Docker doesn't

- **Hardware isolation by default.** A bug in a sandbox kernel can't
  reach the host kernel. Docker shares the host kernel.
- **Sub-second boot.** libkrun's minimal kernel + initramfs + virtio
  devices boot in under a second cold. Cached images: <100ms.
- **Egress allowlist as a first-class field.** Not a daemon-wide
  policy or an iptables rule you wrote — a `CreateSandboxRequest`
  field with a deny-by-default semantic.
- **No daemon-as-root requirement.** wardd runs as the invoking
  user; sandboxes inherit that boundary. Docker historically
  required root or rootless setup with caveats.

## Comparison vs cloud sandboxes (E2B / Daytona / Cloudflare Sandbox)

| Concern | E2B / Daytona | ward |
|---|---|---|
| Where the workload runs | Cloud VM in vendor's account | Local microVM on your host |
| Data residency | Vendor processes it | Never leaves the host |
| Cost model | Per-minute or per-call | Free; bound by your host's resources |
| Egress controls | Vendor-managed | You configure per-sandbox |
| Cold start | ~1-5 seconds | <1s (cached image) |
| Internet required | Yes | No |

Pick ward for: local development, air-gapped environments, latency-sensitive
agents, privacy-sensitive workloads. Pick the cloud option for: workloads
that need to scale beyond your host's capacity, or that genuinely need to
run inside the vendor's network for downstream service access.

## Next steps

- [SECURITY.md](../../SECURITY.md) — what ward isolates and what it doesn't.
- [docs/SPEC.md](../../docs/SPEC.md) — full protocol + ADR index.
- [proto/ward.proto](../../proto/ward.proto) — the gRPC API surface.
- [ADR-008](../../docs/adr/008-egress-control.md) — how egress filtering works.
- Issues [#39](https://github.com/igorjs/ward/issues/39)/[#40](https://github.com/igorjs/ward/issues/40)/[#41](https://github.com/igorjs/ward/issues/41)/[#42](https://github.com/igorjs/ward/issues/42)
  — track the planned Python / TS / Go / Rust SDKs.
