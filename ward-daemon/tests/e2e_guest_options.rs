// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! E2E scenarios for the guest setup flags of `ward run` and `ward create`:
//! what the daemon records and `inspect` shows, and what it refuses. They run
//! the real `ward` binary against a stub-backend `wardd`.

mod common;

use std::process::Output;

use common::Daemon;

const ALPINE: &str = "alpine:latest";
const DEFAULT_SHM_BYTES: &str = "67108864";

fn run(d: &Daemon, args: &[&str]) -> Output {
    d.cli().args(args).output().expect("run ward")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ok(d: &Daemon, args: &[&str]) -> String {
    let out = run(d, args);
    assert!(out.status.success(), "{args:?} failed: {}", stderr(&out));
    stdout(&out)
}

fn inspect(d: &Daemon, template: &str, name: &str) -> String {
    ok(d, &["inspect", "--format", template, name])
        .trim()
        .to_string()
}

#[test]
fn given_read_only_tmpfs_and_shm_size_when_inspected_then_host_config_and_mounts_show_them() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &[
            "create",
            "--name",
            "ro1",
            "--read-only",
            "--tmpfs",
            "/scratch:size=1m,noexec",
            "--tmpfs",
            "/cache",
            "--shm-size",
            "128m",
            ALPINE,
        ],
    );
    ok(&d, &["create", "--name", "plain1", ALPINE]);

    // Act
    let read_only = inspect(&d, "{{.HostConfig.ReadonlyRootfs}}", "ro1");
    let tmpfs = inspect(&d, "{{json .HostConfig.Tmpfs}}", "ro1");
    let shm = inspect(&d, "{{.HostConfig.ShmSize}}", "ro1");
    let mounts = inspect(&d, "{{json .Mounts}}", "ro1");
    let plain_shm = inspect(&d, "{{.HostConfig.ShmSize}}", "plain1");
    let plain_read_only = inspect(&d, "{{.HostConfig.ReadonlyRootfs}}", "plain1");

    // Assert
    assert_eq!(read_only, "true");
    assert_eq!(tmpfs, r#"{"/cache":"","/scratch":"size=1m,noexec"}"#);
    assert_eq!(shm, "134217728");
    assert!(mounts.contains(r#""Destination":"/scratch""#), "{mounts}");
    assert!(mounts.contains(r#""Type":"tmpfs""#), "{mounts}");
    assert_eq!(plain_shm, DEFAULT_SHM_BYTES);
    assert_eq!(plain_read_only, "false");
}

#[test]
fn given_bad_guest_flags_when_created_then_each_fails_with_a_clear_error() {
    // Arrange
    let d = Daemon::spawn();
    let cases: [(&[&str], &str); 5] = [
        (&["--tmpfs", "/x:bogus"], "invalid tmpfs option"),
        (&["--tmpfs", "relative"], "absolute"),
        (&["--tmpfs", "/proc"], "reserved"),
        (
            &["--tmpfs", "/data", "--tmpfs", "/data/"],
            "Duplicate mount point: /data",
        ),
        (&["--shm-size", "lots"], "invalid size"),
    ];

    for (flags, reason) in cases {
        // Act
        let mut args = vec!["create"];
        args.extend(flags);
        args.push(ALPINE);
        let out = run(&d, &args);

        // Assert
        assert!(!out.status.success(), "{flags:?}");
        assert!(stderr(&out).contains(reason), "{flags:?}: {}", stderr(&out));
    }
}
