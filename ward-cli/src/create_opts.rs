// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Create-time flags of `ward run` and `ward create` that Docker and Podman
//! have and that need little more than parsing: env files, labels, a container
//! ID file, `--replace`, `--platform` and `--init`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{CreateSandboxRequest, SandboxInfo};
use ward_core::protocol::{RestartKind, RestartPolicy};

use crate::{client, lifecycle};

/// Looks a variable up on the host.
type HostLookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

const COMMENT_PREFIX: char = '#';
const KEY_VALUE_SEPARATOR: char = '=';
const BYTE_ORDER_MARK: char = '\u{feff}';
const PLATFORM_SEPARATOR: char = '/';
const GUEST_OS: &str = "linux";
const ARM64_VARIANT: &str = "v8";

/// Flags that `create` and `run` share beyond the core sandbox settings.
#[derive(Debug, Args, Default)]
pub struct CreateExtras {
    /// Read environment variables from a file of KEY=VALUE lines. A bare KEY
    /// takes the host value. Repeatable. -e overrides the files, and a later
    /// file overrides an earlier one.
    #[arg(long = "env-file", value_name = "FILE")]
    env_file: Vec<PathBuf>,
    /// Set metadata on the sandbox: KEY or KEY=VALUE. Repeatable.
    #[arg(short = 'l', long = "label", value_name = "KEY[=VALUE]")]
    label: Vec<String>,
    /// Read labels from a file of KEY=VALUE lines. -l overrides the file.
    #[arg(long = "label-file", value_name = "FILE")]
    label_file: Vec<PathBuf>,
    /// Write the sandbox ID to FILE. Fails when FILE already exists.
    #[arg(long, value_name = "FILE")]
    cidfile: Option<PathBuf>,
    /// Remove an existing sandbox with the same --name first, as Podman does.
    #[arg(long)]
    replace: bool,
    /// Host name inside the sandbox (no short form: -h is help). Defaults to
    /// the first 12 characters of the sandbox ID.
    #[arg(long, value_name = "NAME")]
    hostname: Option<String>,
    /// Signal that stops the main process: a name such as SIGINT or a number.
    /// Default SIGTERM.
    #[arg(long = "stop-signal", value_name = "SIGNAL")]
    stop_signal: Option<String>,
    /// Seconds the main process gets after the stop signal before SIGKILL, for
    /// stop, restart and the daemon's shutdown. `stop -t` overrides it.
    #[arg(
        long = "stop-timeout",
        value_name = "SECONDS",
        allow_hyphen_values = true
    )]
    stop_timeout: Option<i32>,
    /// What to do when the main process ends: no (default), on-failure[:N],
    /// always or unless-stopped. Not with --rm.
    #[arg(long, value_name = "POLICY", value_parser = parse_restart)]
    restart: Option<RestartPolicy>,
    /// Accepted for Docker scripts. It has no effect.
    #[arg(long)]
    init: bool,
    /// Platform of the image: linux/ARCH. Ward runs only this host's
    /// architecture.
    #[arg(long, value_name = "linux/ARCH")]
    platform: Option<String>,
}

impl CreateExtras {
    /// Check the flags and put the environment and labels on `request`.
    ///
    /// # Errors
    ///
    /// Fails on an unreadable or malformed file, a bad label or a platform
    /// this host cannot run.
    pub fn apply(&self, request: &mut CreateSandboxRequest, env: &[String]) -> Result<()> {
        check_platform(self.platform.as_deref(), host_arch())?;
        request.env = environment(&self.env_file, env, host_env)?;
        request.labels = labels(&self.label_file, &self.label)?;
        request.hostname = self.hostname.clone().unwrap_or_default();
        if let Some(signal) = &self.stop_signal {
            crate::inspect::parse_signal(signal)
                .map_err(|_| anyhow::anyhow!("invalid --stop-signal: {signal}"))?;
            request.stop_signal = signal.clone();
        }
        request.stop_timeout = self.stop_timeout;
        request.restart_policy = self.restart.map(|policy| ward_core::pb::RestartPolicy {
            name: policy.kind.as_str().to_string(),
            maximum_retry_count: policy.max_retries,
        });
        Ok(())
    }

    /// Refuse `--rm` with a restart policy, as Docker does.
    ///
    /// # Errors
    ///
    /// Fails when `rm` is set and a policy other than `no` is given.
    pub fn check_rm(&self, rm: bool) -> Result<()> {
        let restarts = self.restart.is_some_and(|p| p.kind != RestartKind::No);
        if rm && restarts {
            bail!("conflicting options: --restart and --rm");
        }
        Ok(())
    }

    /// The steps around the create call that need the daemon or the disk.
    pub fn hooks(&self, name: Option<&str>) -> Hooks {
        Hooks {
            cidfile: self.cidfile.clone(),
            replace: self.replace.then(|| name.map(str::to_string)).flatten(),
        }
    }
}

fn parse_restart(text: &str) -> Result<RestartPolicy, String> {
    RestartPolicy::parse(text)
}

fn host_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Docker's name for the CPU architecture this host runs.
fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Accept the platform of this host and refuse any other, so that no image is
/// silently run under the wrong architecture.
fn check_platform(platform: Option<&str>, host_arch: &str) -> Result<()> {
    let Some(platform) = platform.filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    let mut parts = platform.split(PLATFORM_SEPARATOR);
    let os = parts.next().unwrap_or_default();
    let arch = parts.next();
    let variant = parts.next();
    let arch_ok = arch.is_none_or(|arch| arch == host_arch);
    let variant_ok = match (host_arch, variant) {
        (_, None) => true,
        ("arm64", Some(variant)) => variant == ARM64_VARIANT,
        _ => false,
    };
    if os != GUEST_OS || !arch_ok || !variant_ok || parts.next().is_some() {
        bail!(
            "--platform {platform} is not supported: Ward runs {GUEST_OS}/{host_arch} sandboxes on this host"
        );
    }
    Ok(())
}

/// The environment of one command: every env file in order, then each `-e`,
/// with a bare key taking its value from this host.
///
/// # Errors
///
/// Fails on an unreadable or malformed file.
pub fn host_environment(files: &[PathBuf], flags: &[String]) -> Result<HashMap<String, String>> {
    environment(files, flags, host_env)
}

/// The sandbox environment: every env file in order, then each `-e`.
fn environment(
    files: &[PathBuf],
    flags: &[String],
    host: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<String, String>> {
    let mut env = HashMap::new();
    for file in files {
        for (key, value) in read_pairs(file, Some(&host))? {
            env.extend(value.map(|value| (key, value)));
        }
    }
    env.extend(crate::parse_env_with(flags, &host));
    Ok(env)
}

/// The sandbox labels: every label file in order, then each `-l`.
fn labels(files: &[PathBuf], flags: &[String]) -> Result<HashMap<String, String>> {
    let mut labels = HashMap::new();
    for file in files {
        for (key, value) in read_pairs(file, None)? {
            labels.insert(key, value.unwrap_or_default());
        }
    }
    for flag in flags {
        let (key, value) = flag.split_once(KEY_VALUE_SEPARATOR).unwrap_or((flag, ""));
        if key.is_empty() {
            bail!("invalid label {flag:?}: the key is empty");
        }
        labels.insert(key.to_string(), value.to_string());
    }
    Ok(labels)
}

fn read_pairs(path: &Path, host: Option<&HostLookup<'_>>) -> Result<Vec<(String, Option<String>)>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse_pairs(&text, host).with_context(|| format!("in {}", path.display()))
}

/// Docker's file format: `KEY=VALUE` lines, `#` comments and blank lines.
/// A line with no `=` takes its value from `host`, and is dropped when the
/// host has none. Without a `host` it has an empty value.
fn parse_pairs(text: &str, host: Option<&HostLookup<'_>>) -> Result<Vec<(String, Option<String>)>> {
    let mut pairs = Vec::new();
    for line in text.trim_start_matches(BYTE_ORDER_MARK).lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with(COMMENT_PREFIX) {
            continue;
        }
        let (key, value) = match line.split_once(KEY_VALUE_SEPARATOR) {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (line.trim_end(), None),
        };
        if key.is_empty() {
            bail!("no variable name on line '{line}'");
        }
        if key.contains(char::is_whitespace) {
            bail!("variable '{key}' contains whitespaces");
        }
        let value = match (value, host) {
            (Some(value), _) => Some(value),
            (None, Some(host)) => host(key),
            (None, None) => Some(String::new()),
        };
        pairs.push((key.to_string(), value));
    }
    Ok(pairs)
}

/// What happens around the create call.
pub struct Hooks {
    cidfile: Option<PathBuf>,
    replace: Option<String>,
}

impl Hooks {
    /// Claim the ID file, then remove the sandbox `--replace` names.
    ///
    /// # Errors
    ///
    /// Fails when the ID file exists or the old sandbox cannot be removed.
    pub async fn begin(&self, c: &mut WardClient<Channel>, token_store: &Path) -> Result<()> {
        if let Some(path) = &self.cidfile {
            claim_cidfile(path)?;
        }
        if let Some(name) = &self.replace
            && let Err(e) = remove_named(c, token_store, name).await
        {
            self.discard_cidfile();
            return Err(e);
        }
        Ok(())
    }

    /// Write the new ID to the ID file, or delete the file when creation failed.
    pub fn finish(&self, created: anyhow::Result<SandboxInfo>) -> Result<SandboxInfo> {
        match (&self.cidfile, created) {
            (Some(path), Ok(info)) => {
                std::fs::write(path, &info.id)
                    .with_context(|| format!("cannot write {}", path.display()))?;
                Ok(info)
            }
            (Some(_), Err(e)) => {
                self.discard_cidfile();
                Err(e)
            }
            (None, created) => created,
        }
    }

    fn discard_cidfile(&self) {
        if let Some(path) = &self.cidfile {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Create the ID file empty, failing when it exists, as Docker does.
fn claim_cidfile(path: &Path) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.flush().map_err(Into::into),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "container ID file found, make sure the other sandbox isn't running or delete {}",
            path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("cannot create {}", path.display())),
    }
}

/// Remove the sandbox called exactly `name`, running or not. Nothing to do
/// when there is none.
async fn remove_named(c: &mut WardClient<Channel>, token_store: &Path, name: &str) -> Result<()> {
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let Some(info) = listed.iter().find(|info| info.name == name) else {
        return Ok(());
    };
    let target = client::Resolved::from_info(token_store, info)?;
    lifecycle::remove_resolved(c, token_store, &target)
        .await
        .with_context(|| format!("cannot replace sandbox {name}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn given_env_file_and_flags_when_merged_for_exec_then_flags_win() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.env");
        std::fs::write(&file, "A=file\nB=file\n").expect("write");

        // Act
        let env = host_environment(&[file], &["B=flag".to_string()]).expect("env");

        // Assert
        assert_eq!(env["A"], "file");
        assert_eq!(env["B"], "flag");
    }

    use super::*;

    const HOST_VALUE: &str = "from-host";

    fn host(key: &str) -> Option<String> {
        (key == "HOME_VAR").then(|| HOST_VALUE.to_string())
    }

    fn pairs(text: &str) -> Vec<(String, Option<String>)> {
        parse_pairs(text, Some(&host)).expect("parse")
    }

    fn pair(key: &str, value: &str) -> (String, Option<String>) {
        (key.to_string(), Some(value.to_string()))
    }

    #[test]
    fn given_env_file_text_when_parsed_then_comments_and_blank_lines_are_skipped() {
        // Arrange
        let text = "\u{feff}# comment\n\n  A=1\r\nB=two words\nC=\nD=x=y\n";

        // Act
        let parsed = pairs(text);

        // Assert
        assert_eq!(
            parsed,
            vec![
                pair("A", "1"),
                pair("B", "two words"),
                pair("C", ""),
                pair("D", "x=y")
            ]
        );
    }

    #[test]
    fn given_bare_key_when_parsed_then_host_value_is_used_or_the_key_is_dropped() {
        // Act
        let parsed = pairs("HOME_VAR\nUNSET_VAR\n");

        // Assert
        assert_eq!(
            parsed,
            vec![pair("HOME_VAR", HOST_VALUE), ("UNSET_VAR".into(), None)]
        );
    }

    #[test]
    fn given_bare_key_in_a_label_file_when_parsed_then_the_value_is_empty() {
        // Act
        let parsed = parse_pairs("team\n", None).expect("parse");

        // Assert
        assert_eq!(parsed, vec![pair("team", "")]);
    }

    #[rstest::rstest]
    #[case::no_name("=value", "no variable name")]
    #[case::space_in_key("A B=1", "contains whitespaces")]
    fn given_a_bad_line_when_parsed_then_it_is_refused(#[case] text: &str, #[case] reason: &str) {
        // Act
        let error = parse_pairs(text, Some(&host)).expect_err("rejected");

        // Assert
        assert!(error.to_string().contains(reason), "{error}");
    }

    #[test]
    fn given_files_and_flags_when_merged_then_flags_beat_files_and_later_files_beat_earlier() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let first = dir.path().join("first.env");
        let second = dir.path().join("second.env");
        std::fs::write(&first, "A=first\nB=first\nC=first\n").expect("write");
        std::fs::write(&second, "B=second\nC=second\n").expect("write");

        // Act
        let env = environment(&[first, second], &["C=flag".to_string()], host).expect("env");

        // Assert
        assert_eq!(env["A"], "first");
        assert_eq!(env["B"], "second");
        assert_eq!(env["C"], "flag");
    }

    #[test]
    fn given_missing_env_file_when_merged_then_the_error_names_it() {
        // Act
        let error =
            environment(&[PathBuf::from("/nonexistent/x.env")], &[], host).expect_err("missing");

        // Assert
        assert!(error.to_string().contains("/nonexistent/x.env"), "{error}");
    }

    #[test]
    fn given_label_flags_when_merged_then_they_beat_the_file_and_a_bare_key_is_empty() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("labels");
        std::fs::write(&file, "a=file\nb=file\n").expect("write");

        // Act
        let merged = labels(&[file], &["b=flag".to_string(), "c".to_string()]).expect("labels");

        // Assert
        assert_eq!(merged["a"], "file");
        assert_eq!(merged["b"], "flag");
        assert_eq!(merged["c"], "");
    }

    #[test]
    fn given_empty_label_key_when_merged_then_it_is_refused() {
        // Act
        let error = labels(&[], &["=x".to_string()]).expect_err("rejected");

        // Assert
        assert!(error.to_string().contains("key is empty"), "{error}");
    }

    #[rstest::rstest]
    #[case::none(None, "arm64", true)]
    #[case::empty(Some(""), "arm64", true)]
    #[case::os_only(Some("linux"), "amd64", true)]
    #[case::host(Some("linux/arm64"), "arm64", true)]
    #[case::variant(Some("linux/arm64/v8"), "arm64", true)]
    #[case::other_arch(Some("linux/amd64"), "arm64", false)]
    #[case::other_os(Some("windows/amd64"), "amd64", false)]
    #[case::bad_variant(Some("linux/arm64/v7"), "arm64", false)]
    #[case::amd64_variant(Some("linux/amd64/v2"), "amd64", false)]
    #[case::extra_part(Some("linux/arm64/v8/x"), "arm64", false)]
    fn given_a_platform_when_checked_then_only_this_hosts_is_accepted(
        #[case] platform: Option<&str>,
        #[case] arch: &str,
        #[case] accepted: bool,
    ) {
        // Act
        let result = check_platform(platform, arch);

        // Assert
        assert_eq!(result.is_ok(), accepted, "{result:?}");
    }

    #[test]
    fn given_existing_cidfile_when_claimed_then_it_is_refused_and_left_alone() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cid");
        std::fs::write(&path, "old").expect("write");

        // Act
        let error = claim_cidfile(&path).expect_err("exists");

        // Assert
        assert!(error.to_string().contains("ID file found"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "old");
    }

    #[test]
    fn given_claimed_cidfile_when_creation_succeeds_then_it_holds_the_id_without_newline() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cid");
        let hooks = Hooks {
            cidfile: Some(path.clone()),
            replace: None,
        };
        claim_cidfile(&path).expect("claim");
        let info = SandboxInfo {
            id: "abc123".into(),
            ..Default::default()
        };

        // Act
        hooks.finish(Ok(info)).expect("finish");

        // Assert
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "abc123");
    }

    #[test]
    fn given_claimed_cidfile_when_creation_fails_then_the_file_is_removed() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cid");
        let hooks = Hooks {
            cidfile: Some(path.clone()),
            replace: None,
        };
        claim_cidfile(&path).expect("claim");

        // Act
        let result = hooks.finish(Err(anyhow::anyhow!("boom")));

        // Assert
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn given_stop_and_host_flags_when_applied_then_the_request_carries_them() {
        // Arrange
        let extras = CreateExtras {
            hostname: Some("web1".into()),
            stop_signal: Some("SIGINT".into()),
            stop_timeout: Some(-1),
            ..Default::default()
        };
        let mut request = CreateSandboxRequest::default();

        // Act
        extras.apply(&mut request, &[]).expect("apply");

        // Assert
        assert_eq!(request.hostname, "web1");
        assert_eq!(request.stop_signal, "SIGINT");
        assert_eq!(request.stop_timeout, Some(-1));
    }

    #[test]
    fn given_unknown_stop_signal_when_applied_then_it_is_refused_before_the_wire() {
        // Arrange
        let extras = CreateExtras {
            stop_signal: Some("SIGNOPE".into()),
            ..Default::default()
        };

        // Act
        let error = extras
            .apply(&mut CreateSandboxRequest::default(), &[])
            .expect_err("rejected");

        // Assert
        assert!(error.to_string().contains("--stop-signal"), "{error}");
    }

    #[rstest::rstest]
    #[case::none(None, true, true)]
    #[case::no(Some("no"), true, true)]
    #[case::always_without_rm(Some("always"), false, true)]
    #[case::always_with_rm(Some("always"), true, false)]
    #[case::on_failure_with_rm(Some("on-failure:3"), true, false)]
    fn given_restart_and_rm_when_checked_then_only_a_real_policy_conflicts(
        #[case] restart: Option<&str>,
        #[case] rm: bool,
        #[case] ok: bool,
    ) {
        // Arrange
        let extras = CreateExtras {
            restart: restart.map(|text| RestartPolicy::parse(text).expect("policy")),
            ..Default::default()
        };

        // Act
        let result = extras.check_rm(rm);

        // Assert
        assert_eq!(result.is_ok(), ok);
    }

    #[test]
    fn given_restart_flag_when_applied_then_the_request_carries_the_policy() {
        // Arrange
        let extras = CreateExtras {
            restart: Some(RestartPolicy::parse("on-failure:3").expect("policy")),
            ..Default::default()
        };
        let mut request = CreateSandboxRequest::default();

        // Act
        extras.apply(&mut request, &[]).expect("apply");

        // Assert
        let policy = request.restart_policy.expect("policy");
        assert_eq!(policy.name, "on-failure");
        assert_eq!(policy.maximum_retry_count, 3);
    }

    #[test]
    fn given_replace_without_a_name_when_hooks_built_then_nothing_is_replaced() {
        // Arrange
        let extras = CreateExtras {
            replace: true,
            ..Default::default()
        };

        // Act
        let hooks = extras.hooks(None);

        // Assert
        assert!(hooks.replace.is_none());
    }
}
