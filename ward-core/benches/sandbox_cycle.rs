// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Sandbox create plus remove cycle on the stub backend. This is the
//! manager and bookkeeping baseline, with no VM boot in the number.

mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Runtime;

use ward_core::pb::CreateSandboxRequest;

const IMAGE: &str = "alpine:latest";
// Only consulted for snapshot clones, so empty for a plain create.
const NO_TOKEN: &str = "";

fn bench_cycle(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let (sandboxes, _volumes) = common::managers(dir.path());

    c.bench_function("sandbox_create_remove_stub", |b| {
        b.to_async(&rt).iter(|| async {
            let request = CreateSandboxRequest {
                image: IMAGE.into(),
                ..Default::default()
            };
            let info = sandboxes.create(request, NO_TOKEN).await.expect("create");
            sandboxes.remove(&info.id).await.expect("remove");
        });
    });
}

criterion_group!(benches, bench_cycle);
criterion_main!(benches);
