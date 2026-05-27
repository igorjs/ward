"""Thin Pythonic wrapper over the ward gRPC API.

This module intentionally hides the protobuf types from the call site;
users pass dicts / kwargs / dataclasses and get back simple dataclasses.
Advanced callers can drop into ``ward_sdk.proto`` for full access.

Note: the protobuf stubs (``ward_sdk.proto``) are generated at install
time via ``grpcio-tools`` (see README). Until that step runs, the
``WardClient`` methods raise NotImplementedError — treat this file as
the SHAPE of the SDK rather than a turn-key import path.
"""

from __future__ import annotations

import os
import pathlib
from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from typing import Iterator, Optional, Sequence


def default_socket_path() -> pathlib.Path:
    """Resolve the path wardd is listening on for the current user.

    Mirrors ``ward-core/src/config.rs::default_socket_path``:
    - macOS: ``$HOME/.ward/ward.sock``
    - Linux: ``$XDG_RUNTIME_DIR/ward/ward.sock`` if set, else
      ``$HOME/.ward/ward.sock``
    """
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        return pathlib.Path(xdg) / "ward" / "ward.sock"
    home = os.environ.get("HOME")
    if not home:
        raise RuntimeError("HOME is not set; cannot resolve default ward socket path")
    return pathlib.Path(home) / ".ward" / "ward.sock"


# ─── Dataclasses returned by the client ──────────────────────────────


@dataclass
class Sandbox:
    """One ward sandbox. Returned by ``WardClient.create_sandbox``."""

    id: str
    image: str
    status: str
    # Add fields as the SDK grows. Keeping it thin so the surface area
    # is easy to track.


@dataclass
class ExecResult:
    """Result of a fire-and-forget ``WardClient.run`` call."""

    pid: str
    stdout: str = ""
    stderr: str = ""
    exit_code: Optional[int] = None


@dataclass
class StreamEvent:
    """One event from ``WardClient.stream_output``."""

    kind: str  # "stdout" | "stderr" | "exit"
    line: str = ""
    exit_code: Optional[int] = None
    duration_ms: int = 0


# ─── Client ──────────────────────────────────────────────────────────


@dataclass
class WardClient(AbstractContextManager["WardClient"]):
    """gRPC client for the ward daemon.

    Construct via one of the class methods rather than the bare
    constructor so the socket-path / TCP-target decision is explicit.
    """

    socket_path: Optional[pathlib.Path] = None
    tcp_target: Optional[str] = None
    # Lazily-initialised gRPC channel + stub. Set in ``_connect`` so
    # construction is cheap (useful for tests and lazy CLIs).
    _channel: object = field(default=None, repr=False, compare=False)
    _stub: object = field(default=None, repr=False, compare=False)

    @classmethod
    def connect(cls, socket_path: Optional[pathlib.Path] = None) -> "WardClient":
        """Connect over a Unix domain socket.

        Defaults to the daemon's configured socket path for the current
        user. Override with ``socket_path=`` to point at a different
        wardd instance (e.g. a per-test ephemeral daemon).
        """
        return cls(socket_path=socket_path or default_socket_path())

    @classmethod
    def connect_tcp(cls, target: str) -> "WardClient":
        """Connect over TCP. Requires daemon-side mTLS / token auth
        (ADR-013) which is not yet implemented in wardd."""
        return cls(tcp_target=target)

    # ── Sandbox lifecycle ───────────────────────────────────────────

    def create_sandbox(
        self,
        image: str,
        *,
        egress: str = "deny",
        cpus: int = 1,
        memory_mb: int = 512,
        timeout_seconds: int = 60,
        env: Optional[dict[str, str]] = None,
    ) -> Sandbox:
        """Create a fresh sandbox from an OCI image.

        Defaults match the AI-agent-sandbox profile documented in
        ``examples/ai-agent-sandbox/README.md``: deny-by-default egress,
        modest CPU/memory caps, 60-second wall-clock timeout.
        """
        raise NotImplementedError("first-cut scaffold; wire to gRPC stub when proto codegen lands")

    def remove_sandbox(self, sandbox_id: str) -> None:
        """Tear a sandbox down. Idempotent: removing an already-gone
        sandbox is a no-op success."""
        raise NotImplementedError("first-cut scaffold; wire to gRPC stub when proto codegen lands")

    def sandbox(self, image: str, **kwargs: object) -> "_SandboxContext":
        """Context manager that creates a sandbox on entry and removes
        it on exit (even on exception)."""
        return _SandboxContext(self, image, kwargs)

    # ── Process operations ──────────────────────────────────────────

    def run(
        self,
        sandbox_id: str,
        argv: Sequence[str],
        *,
        working_dir: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
    ) -> ExecResult:
        """Run a command in the sandbox. Returns when the process exits.

        For long-running commands, prefer ``stream_output`` to consume
        stdout/stderr as they're produced.
        """
        raise NotImplementedError("first-cut scaffold; wire to gRPC stub when proto codegen lands")

    def stream_output(self, sandbox_id: str, pid: str) -> Iterator[StreamEvent]:
        """Stream stdout / stderr / exit events from a running process.

        Iteration ends when the process exits and the final ``exit``
        event has been yielded.
        """
        raise NotImplementedError("first-cut scaffold; wire to gRPC stub when proto codegen lands")

    # ── Context manager ─────────────────────────────────────────────

    def close(self) -> None:
        """Close the underlying gRPC channel. Called automatically on
        context-manager exit."""
        # Will call _channel.close() once _channel is wired up.
        return None

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        self.close()


@dataclass
class _SandboxContext(AbstractContextManager[Sandbox]):
    """Helper for ``with client.sandbox(...) as sb:`` syntax."""

    client: WardClient
    image: str
    kwargs: dict[str, object]
    _sandbox: Optional[Sandbox] = None

    def __enter__(self) -> Sandbox:
        self._sandbox = self.client.create_sandbox(self.image, **self.kwargs)  # type: ignore[arg-type]
        return self._sandbox

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        if self._sandbox is not None:
            self.client.remove_sandbox(self._sandbox.id)
