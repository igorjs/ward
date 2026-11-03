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
    let cases: [(&[&str], &str); 6] = [
        (&["--tmpfs", "/x:bogus"], "invalid tmpfs option"),
        (&["--tmpfs", "relative"], "absolute"),
        (&["--tmpfs", "/proc"], "reserved"),
        (
            &["--tmpfs", "/data", "--mount", "type=tmpfs,dst=/data"],
            "Duplicate mount point: /data",
        ),
        (&["--mount", "type=volume,src=v,dst=/v"], "named volumes"),
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

#[test]
fn given_mount_flag_when_inspected_then_bind_and_tmpfs_map_onto_the_existing_mounts() {
    // Arrange
    let d = Daemon::spawn();
    // The stub daemon only shares paths under its default allowlist.
    let dir = tempfile::tempdir_in("/tmp").expect("tempdir");
    let source = dir.path().to_str().unwrap().to_string();
    let bind = format!("type=bind,src={source},dst=/data,readonly");
    ok(
        &d,
        &[
            "create",
            "--name",
            "m1",
            "--mount",
            &bind,
            "--mount",
            "type=tmpfs,dst=/cache,tmpfs-size=1m",
            "-v",
            &format!("{source}:/other"),
            ALPINE,
        ],
    );

    // Act
    let tmpfs = inspect(&d, "{{json .HostConfig.Tmpfs}}", "m1");
    let mounts = inspect(&d, "{{json .Mounts}}", "m1");

    // Assert
    assert!(mounts.contains(r#""Mode":"ro""#), "{mounts}");
    assert!(mounts.contains(r#""Destination":"/other""#), "{mounts}");
    assert_eq!(tmpfs, r#"{"/cache":"rw,size=1048576"}"#);
}

#[test]
fn given_hosts_and_resolver_flags_when_inspected_then_host_config_shows_them() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &[
            "create",
            "--name",
            "net1",
            "--add-host",
            "db:10.1.2.3",
            "--add-host",
            "v6:[::1]",
            "--add-host",
            "gw:host-gateway",
            "--dns-search",
            "example.com",
            "--dns-option",
            "ndots:2",
            ALPINE,
        ],
    );

    // Act
    let hosts = inspect(&d, "{{json .HostConfig.ExtraHosts}}", "net1");
    let search = inspect(&d, "{{json .HostConfig.DnsSearch}}", "net1");
    let options = inspect(&d, "{{json .HostConfig.DnsOptions}}", "net1");

    // Assert
    assert_eq!(hosts, r#"["db:10.1.2.3","v6:::1","gw:10.0.2.2"]"#);
    assert_eq!(search, r#"["example.com"]"#);
    assert_eq!(options, r#"["ndots:2"]"#);
}

#[test]
fn given_bad_hosts_or_dns_flags_when_created_then_each_fails_with_a_clear_error() {
    // Arrange
    let d = Daemon::spawn();
    let cases: [(&[&str], &str); 4] = [
        (&["--add-host", "db"], "use HOST:IP"),
        (&["--add-host", "db:nope"], "not an IP"),
        (&["--dns-option", "ndots 2"], "invalid --dns-option"),
        (&["--dns", "1.1.1.1"], "--egress"),
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

#[test]
fn given_limit_flags_when_inspected_then_host_config_shows_them() {
    // Arrange
    let d = Daemon::spawn();
    ok(
        &d,
        &[
            "create",
            "--name",
            "lim1",
            "-m",
            "64m",
            "--ulimit",
            "nofile=1024:2048",
            "--ulimit",
            "core=0",
            "--group-add",
            "audio",
            "--group-add",
            "1234",
            "--memory-swap",
            "64m",
            "--memory-reservation",
            "32m",
            "--cpu-shares",
            "512",
            "--cpuset-cpus",
            "0-1",
            "--cpu-period",
            "50000",
            "--cpu-quota",
            "25000",
            ALPINE,
        ],
    );

    // Act
    let ulimits = inspect(&d, "{{json .HostConfig.Ulimits}}", "lim1");
    let groups = inspect(&d, "{{json .HostConfig.GroupAdd}}", "lim1");
    let swap = inspect(&d, "{{.HostConfig.MemorySwap}}", "lim1");
    let reservation = inspect(&d, "{{.HostConfig.MemoryReservation}}", "lim1");
    let shares = inspect(&d, "{{.HostConfig.CpuShares}}", "lim1");
    let cpuset = inspect(&d, "{{.HostConfig.CpusetCpus}}", "lim1");
    let quota = inspect(&d, "{{.HostConfig.CpuQuota}}", "lim1");

    // Assert
    assert!(ulimits.contains(r#""Name":"nofile""#), "{ulimits}");
    assert!(ulimits.contains(r#""Hard":2048"#), "{ulimits}");
    assert_eq!(groups, r#"["audio","1234"]"#);
    assert_eq!(swap, "67108864");
    assert_eq!(reservation, "33554432");
    assert_eq!(shares, "512");
    assert_eq!(cpuset, "0-1");
    assert_eq!(quota, "25000");
}

#[test]
fn given_bad_limit_flags_when_created_then_each_fails_with_a_clear_error() {
    // Arrange
    let d = Daemon::spawn();
    let cases: [(&[&str], &str); 7] = [
        (&["--ulimit", "files=1"], "invalid ulimit type"),
        (&["--ulimit", "nofile=10:5"], "soft limit"),
        (&["--group-add", "bad group"], "invalid --group-add"),
        (&["-m", "64m", "--memory-swap", "128m"], "no swap"),
        (&["--memory-swap", "-1"], "no swap"),
        (&["--cpu-shares", "1"], "--cpu-shares"),
        (&["--cpuset-cpus", "x"], "--cpuset-cpus"),
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
