// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Broker publish fan-out throughput against subscriber count.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;

use ward_core::comms::Broker;
use ward_core::protocol::{CommunicationMode, CommunicationPolicy};

const SUBSCRIBER_COUNTS: [usize; 4] = [1, 10, 100, 1000];
const PUBLISHER: &str = "publisher";
const TOPIC: &str = "bench";
const GROUP: &str = "bench-group";
const PAYLOAD_BYTES: usize = 256;

fn group_policy() -> CommunicationPolicy {
    CommunicationPolicy {
        mode: CommunicationMode::Group,
        group: Some(GROUP.to_string()),
    }
}

fn bench_fan_out(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let payload = Bytes::from(vec![0u8; PAYLOAD_BYTES]);
    let mut group = c.benchmark_group("broker_publish_fan_out");

    for count in SUBSCRIBER_COUNTS {
        // Arrange: one publisher and `count` subscribers on one topic. The
        // receivers stay alive but undrained, so once a buffer fills the
        // publish takes the lossy drop path, which is the steady state.
        let (broker, _receivers) = rt.block_on(async {
            let broker = Broker::new();
            broker
                .register_sandbox(PUBLISHER.to_string(), group_policy())
                .await;
            let mut receivers = Vec::with_capacity(count);
            for i in 0..count {
                let id = format!("subscriber-{i}");
                broker.register_sandbox(id.clone(), group_policy()).await;
                receivers.push(broker.subscribe(&id, TOPIC).await.expect("subscribe"));
            }
            (broker, receivers)
        });

        // Throughput is deliveries per second: one publish reaches `count`.
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            // Act
            b.to_async(&rt)
                .iter(|| broker.publish(PUBLISHER, TOPIC, payload.clone()));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_fan_out);
criterion_main!(benches);
