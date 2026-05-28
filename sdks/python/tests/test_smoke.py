"""Smoke tests for ward-sdk.

These confirm the package surface compiles + imports correctly. They
do NOT exercise the gRPC stubs (that requires a running wardd, which
belongs in integration tests rather than unit tests). When the proto
codegen lands and ``WardClient`` is wired up, add round-trip tests
that spawn a wardd via ``subprocess`` and exercise create/exec/remove.
"""

from __future__ import annotations

import pathlib
import sys

import pytest

import ward_sdk
from ward_sdk import ExecResult, Sandbox, StreamEvent, WardClient, default_socket_path


def test_package_version_is_string() -> None:
    assert isinstance(ward_sdk.__version__, str)
    assert ward_sdk.__version__.count(".") >= 1


@pytest.mark.skipif(sys.platform != "linux", reason="Linux-only XDG path")
def test_default_socket_path_uses_xdg_runtime_dir_on_linux(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("XDG_RUNTIME_DIR", "/run/user/1000")
    monkeypatch.setenv("USER", "test")
    assert default_socket_path() == pathlib.Path("/run/user/1000/ward/ward.sock")


@pytest.mark.skipif(sys.platform != "linux", reason="Linux-only /tmp fallback")
def test_default_socket_path_falls_back_to_tmp_user_on_linux(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # XDG unset on Linux must NOT fall back to $HOME; the Rust daemon
    # uses /tmp/ward-$USER in that branch, so the SDK has to match.
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.setenv("USER", "alice")
    assert default_socket_path() == pathlib.Path("/tmp/ward-alice/ward.sock")


@pytest.mark.skipif(sys.platform == "linux", reason="macOS / other Unix branch")
def test_default_socket_path_uses_home_on_macos(monkeypatch: pytest.MonkeyPatch) -> None:
    # macOS ignores XDG even if set; the Rust daemon does too.
    monkeypatch.setenv("XDG_RUNTIME_DIR", "/run/user/1000")
    monkeypatch.setenv("HOME", "/Users/test")
    assert default_socket_path() == pathlib.Path("/Users/test/.ward/ward.sock")


@pytest.mark.skipif(sys.platform == "linux", reason="HOME-required branch is macOS / other")
def test_default_socket_path_raises_when_home_unset(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    monkeypatch.delenv("HOME", raising=False)
    with pytest.raises(RuntimeError, match="HOME"):
        default_socket_path()


def test_client_connect_defaults_to_platform_socket(monkeypatch: pytest.MonkeyPatch) -> None:
    # Cross-platform: assert connect() round-trips whatever default_socket_path
    # produces for this platform. The platform-specific value is covered by the
    # cases above.
    monkeypatch.setenv("HOME", "/home/test")
    monkeypatch.setenv("USER", "test")
    monkeypatch.setenv("XDG_RUNTIME_DIR", "/run/user/1000")
    expected = default_socket_path()
    client = WardClient.connect()
    assert client.socket_path == expected
    assert client.tcp_target is None


def test_client_connect_tcp_sets_target() -> None:
    client = WardClient.connect_tcp("127.0.0.1:9091")
    assert client.socket_path is None
    assert client.tcp_target == "127.0.0.1:9091"


def test_dataclasses_round_trip_via_kwargs() -> None:
    # Sandbox / ExecResult / StreamEvent are dataclasses so callers can
    # construct them in tests without going through the gRPC layer.
    sb = Sandbox(id="sb_01", image="alpine:latest", status="running")
    assert sb.id == "sb_01"

    res = ExecResult(pid="pr_01", stdout="hi\n", stderr="", exit_code=0)
    assert res.exit_code == 0

    ev = StreamEvent(kind="stdout", line="hello")
    assert ev.kind == "stdout"
    assert ev.exit_code is None
