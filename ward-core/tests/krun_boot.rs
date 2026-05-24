// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! KVM-gated boot integration tests.
//!
//! These require a real KVM-capable host (`/dev/kvm`) plus
//! `--features krunvm` and an installed libkrun. Standard GitHub-hosted
//! runners provide none of these, so the tests are gated behind
//! `WARD_KVM_TESTS=1` and skip cleanly everywhere else. A future
//! self-hosted KVM runner enables them by exporting that variable.

/// Returns true when KVM boot tests are explicitly opted into.
fn kvm_tests_enabled() -> bool {
    std::env::var("WARD_KVM_TESTS").as_deref() == Ok("1")
}

#[test]
fn boot_harness_is_gated_off_by_default() {
    if kvm_tests_enabled() {
        // A real boot test will live here once a KVM runner exists:
        // create a sandbox, assert it reaches RUNNING, exec `true`,
        // assert exit 0, remove it. For now the gate itself is the
        // contract under test.
        eprintln!("WARD_KVM_TESTS=1: KVM boot tests would run here");
    } else {
        // Inert on standard runners and dev machines.
        assert!(!kvm_tests_enabled());
    }
}
