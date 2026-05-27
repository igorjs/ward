# ward-sdk (Python)

Python client for the [ward](https://github.com/igorjs/ward) sandbox daemon.
Wraps the gRPC API at [`proto/ward.proto`](../../proto/ward.proto) into a
small, opinionated `WardClient` class.

> **Status: pre-release, first-cut scaffold.** The package layout, install
> path, and `WardClient` shape are stable; the actual protobuf code
> generation is wired but the high-level helpers are minimal. Track
> [issue #39](https://github.com/igorjs/ward/issues/39).

## Install (when published)

```sh
pip install ward-sdk
```

For now (local development):

```sh
cd sdks/python
pip install -e ".[dev]"
python -m grpc_tools.protoc \
    --proto_path=../../proto \
    --python_out=ward_sdk/proto \
    --grpc_python_out=ward_sdk/proto \
    ../../proto/ward.proto
```

## Quick start

```python
from ward_sdk import WardClient

# Connect to the local wardd (defaults to ~/.ward/ward.sock).
client = WardClient.connect()

# Create a sandbox.
sb = client.create_sandbox(image="alpine:latest", egress="deny")

# Run untrusted code in it.
result = client.run(sb.id, ["echo", "hello from inside"])
print(result.stdout)

# Tear down.
client.remove_sandbox(sb.id)
```

For long-running output, stream events:

```python
for event in client.stream_output(sb.id, pid=result.pid):
    if event.kind == "stdout":
        print(event.line)
    elif event.kind == "exit":
        print(f"finished with code {event.exit_code}")
        break
```

## Context manager

`WardClient` is also a context manager that cleans up the underlying gRPC
channel on exit, and `Sandbox` exposes the same to tear itself down:

```python
with WardClient.connect() as client, client.sandbox("alpine:latest") as sb:
    print(sb.run(["uname", "-a"]).stdout)
# sandbox removed here, even on exception
```

## Configuration

| Constructor | Behaviour |
|---|---|
| `WardClient.connect()` | Connect to `$HOME/.ward/ward.sock` (or `$XDG_RUNTIME_DIR/ward/ward.sock` on Linux) |
| `WardClient.connect(socket_path="/path/to/sock")` | Connect to an explicit Unix socket |
| `WardClient.connect_tcp("host:port")` | Connect over TCP — requires the daemon-side TCP/auth work from [ADR-013](../../docs/adr/013-multi-tenant-auth.md) (not yet implemented) |

## Why a Python SDK

ward's lead use case per its README is "AI agent sandboxing". Most AI / LLM
tooling is Python — autonomous coding agents (Claude Code, OpenInterpreter,
Aider), Jupyter notebook drivers, LangChain / LlamaIndex / DSPy integrations,
function-calling backends. A first-class Python wrapper is the path of
least resistance from "I'm building an agent" to "and I want it sandboxed".

For other languages:

- TypeScript SDK — [#40](https://github.com/igorjs/ward/issues/40)
- Go SDK — [#41](https://github.com/igorjs/ward/issues/41)
- Rust SDK — [#42](https://github.com/igorjs/ward/issues/42)

## Licence

Apache-2.0. Independent of ward's AGPL-3.0 daemon licence so SDK consumers
can use ward's API without inheriting copyleft on their own code (per
[ADR-005](../../docs/adr/005-sdk-strategy.md) and
[ADR-006](../../docs/adr/006-licensing.md)).
