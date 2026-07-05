# SPDX-License-Identifier: Apache-2.0

"""ward-sdk: Python client for the ward sandbox daemon.

Top-level imports re-export the surface most callers need:

    from ward_sdk import WardClient, Sandbox, ExecResult, StreamEvent

Lower-level protobuf types live under ``ward_sdk.proto`` once the proto
codegen has been run; they're useful for advanced usage (e.g. building
your own request objects) but most callers stay at the WardClient layer.
"""

from __future__ import annotations

from ward_sdk.client import (
    ExecResult,
    Sandbox,
    StreamEvent,
    WardClient,
    default_socket_path,
)

__all__ = [
    "ExecResult",
    "Sandbox",
    "StreamEvent",
    "WardClient",
    "default_socket_path",
]

__version__ = "0.1.0"
