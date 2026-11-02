<!-- Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only -->

# Benchmarks

Criterion benches live in `ward-core/benches/`:

| Bench | What it measures |
|-------|------------------|
| `broker` | `Broker::publish` fan-out against subscriber count (1, 10, 100, 1000) |
| `grpc` | `GetHealth` round trip over a Unix socket and over TCP loopback, in-process tonic server on the stub backend |
| `sandbox_cycle` | `SandboxManager` create plus remove on the stub backend (no VM boot) |

## Run

```sh
cargo bench -p ward-core --bench broker --bench grpc --bench sandbox_cycle
```

Pass `--bench <name>` rather than `--benches`: the library target would reject
criterion flags. A quick run:

```sh
cargo bench -p ward-core --bench broker -- --warm-up-time 1 --measurement-time 3
```

## Compare

```sh
git switch main
cargo bench -p ward-core --bench broker --bench grpc --bench sandbox_cycle -- --save-baseline main
git switch my-branch
cargo bench -p ward-core --bench broker --bench grpc --bench sandbox_cycle -- --baseline main
```

Criterion keeps raw data under `target/criterion`, which is not checked in.
Only the table below is.

## Baseline

Measured with `--warm-up-time 1 --measurement-time 3`, release profile, stub
backend. Time is the criterion point estimate (middle value).

- Machine: Apple M1 Pro, 16 GiB RAM
- OS: macOS 15.8.1 (Darwin 24.6.0, arm64)
- Rust: 1.99.1

| Bench | Time | Throughput |
|-------|------|------------|
| `broker_publish_fan_out/1` | 353 ns | 2.83 Melem/s |
| `broker_publish_fan_out/10` | 1.27 µs | 7.89 Melem/s |
| `broker_publish_fan_out/100` | 10.6 µs | 9.39 Melem/s |
| `broker_publish_fan_out/1000` | 109 µs | 9.21 Melem/s |
| `grpc_get_health_round_trip/unix_socket` | 40.9 µs | |
| `grpc_get_health_round_trip/tcp_loopback` | 53.4 µs | |
| `sandbox_create_remove_stub` | 1.14 ms | |

Fan-out throughput counts deliveries per second. Subscribers are never
drained, so after the first few publishes each buffer is full and the publish
takes the lossy drop path, which is the steady state under load.

Numbers vary by machine and load. Compare runs on one machine against a saved
baseline rather than against this table.
