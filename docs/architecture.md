# Architecture

```
                                                +------------------+
                                                |   ward (CLI)     |
                                                +--------+---------+
                                                         |
                                                         | gRPC over Unix socket
                                                         v
+---------+   pull + unpack    +----------------------------------------+
|  OCI    |  ----------------> |               wardd (daemon)           |
| images  |                    |                                        |
+---------+                    |  +----------+   +--------+   +-------+ |
                               |  |  Sandbox |   | Comms  |   | Egress| |
                               |  | Manager  |   | Broker |   | Proxy | |
                               |  +-----+----+   +---+----+   +---+---+ |
                               |        |            |            |     |
                               |        v            v            v     |
                               |  +-----------------------------------+ |
                               |  |        Backend trait              | |
                               |  |  (today: libkrun via krun_ffi)    | |
                               |  +-----+-----------------------------+ |
                               +--------|-------------------------------+
                                        |
                                        v
                              +------------------+
                              |   microVM A      |  Linux kernel
                              |   microVM B      |  Linux kernel
                              |   microVM C      |  Linux kernel
                              +------------------+
```

Per-layer rationale lives in the ADRs under [`adr/`](adr/). [`SPEC.md`](SPEC.md)
is the table of contents. Good starting points:

- [ADR-001](adr/001-project-scope.md): what's in and out of scope
- [ADR-003](adr/003-isolation-backend.md): libkrun + the `krunvm` flag
- [ADR-004](adr/004-ipc-protocol.md): gRPC + proto schema
- [ADR-008](adr/008-egress-control.md): egress filtering model
- [ADR-011](adr/011-cross-sandbox-comms.md): pub/sub broker
- [ADR-012](adr/012-backend-trait.md): backend trait abstraction
