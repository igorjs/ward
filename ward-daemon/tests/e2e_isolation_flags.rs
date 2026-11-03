// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The Docker isolation flags a microVM has no use for are refused by
//! `ward create` and `ward run` with a reason and a pointer to the docs.

mod common;

use common::Daemon;

const ALPINE: &str = "alpine:latest";

#[test]
fn given_isolation_flags_when_created_then_each_is_refused_with_the_docs_pointer() {
    // Arrange
    let d = Daemon::spawn();
    let flags: [&[&str]; 12] = [
        &["--privileged"],
        &["--cap-add", "NET_ADMIN"],
        &["--cap-drop", "ALL"],
        &["--security-opt", "seccomp=unconfined"],
        &["--device", "/dev/fuse"],
        &["--network", "host"],
        &["--net", "host"],
        &["--ipc", "host"],
        &["--pid", "host"],
        &["--uts", "host"],
        &["--userns", "host"],
        &["--cgroupns", "host"],
    ];

    for flag in flags {
        for command in ["create", "run"] {
            // Act
            let mut args = vec![command];
            args.extend(flag);
            args.push(ALPINE);
            let out = d.cli().args(&args).output().expect("run ward");

            // Assert
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{args:?}");
            assert!(error.contains("is not supported"), "{args:?}: {error}");
            assert!(error.contains("docs/docker-differences.md"), "{error}");
        }
    }
}

#[test]
fn given_privileged_false_when_created_then_it_is_accepted() {
    // Arrange
    let d = Daemon::spawn();

    // Act
    let out = d
        .cli()
        .args(["create", "--privileged=false", ALPINE])
        .output()
        .expect("run ward");

    // Assert
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
