# Status and roadmap

Live status, tickets, and priorities live on the
[Ward project board](https://github.com/users/igorjs/projects/2). This page
is a periodic snapshot.

- Stub backend: complete, 300+ tests passing
- libkrun FFI surface: complete (60 symbols, hand-maintained)
- VM lifecycle wiring (`krun_start_enter` + shutdown signalling): complete
- OCI image pull + unpack: complete
- Guest agent (`ward-agent`): crate + vsock protocol shipped; boot-path
  integration tracked by [#9](https://github.com/igorjs/ward/issues/9)
- Cross-sandbox pub/sub broker: complete (deny default + group routing)
- Egress: forward proxy + `GetEgressLog` shipped; in-VM TAP routing gated
- Volumes: fixed-size ext4 images shipped; block-attach blocked on libkrun
  built with `--enable-blk`
  ([#43](https://github.com/igorjs/ward/issues/43))
- Snapshots: disk-level archive/restore + `from_snapshot` shipped; live
  checkpoint/restore tracked by
  [#29](https://github.com/igorjs/ward/issues/29)
- First signed release (v0.1.0): release smoke-test
  ([#4](https://github.com/igorjs/ward/issues/4)), then cut v0.1.0
  ([#5](https://github.com/igorjs/ward/issues/5))
