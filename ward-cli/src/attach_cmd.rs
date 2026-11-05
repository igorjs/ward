// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward attach SANDBOX`: connects the terminal to the main process of a
//! running sandbox, as `docker attach` does. The exit code is the process's.

use std::path::Path;

use anyhow::{Result, bail};
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{AttachMode, AttachStart, SandboxStatus};

use crate::attach::{self, Options, Output, require_exit, require_terminal};
use crate::client::{self, Resolved};
use crate::detach_keys::{DEFAULT_SEQUENCE, DetachKeys};

/// Docker's wording for a sandbox that is not running.
const NOT_RUNNING: &str = "You cannot attach to a stopped container, start it first";

/// The flags `attach` and `start --attach` share.
#[derive(clap::Args, Debug)]
pub struct SessionFlags {
    /// Key sequence that detaches and leaves the process running: keys
    /// separated by commas, each a character or ctrl- plus a letter, @, [,
    /// \, ], ^ or _.
    #[arg(long, value_name = "SEQUENCE", default_value = DEFAULT_SEQUENCE)]
    pub detach_keys: String,
    /// Pass SIGINT and SIGTERM to the process. Needs no terminal: with a
    /// terminal, Ctrl-C is sent as a key.
    #[arg(
        long,
        default_value_t = true,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    pub sig_proxy: bool,
}

/// `--detach-keys` of `run` and `exec`. Unlike `attach`, no key sequence is
/// active unless one is given.
#[derive(clap::Args, Debug, Default)]
pub struct DetachFlag {
    /// Key sequence that detaches and leaves the process running: keys
    /// separated by commas, each a character or ctrl- plus a letter, @, [,
    /// \, ], ^ or _. Off unless given (Docker's default is ctrl-p,ctrl-q).
    #[arg(long, value_name = "SEQUENCE")]
    pub detach_keys: Option<String>,
}

impl DetachFlag {
    /// The attach settings for `sig_proxy` and these keys.
    ///
    /// # Errors
    ///
    /// Fails when the key sequence is not valid.
    pub fn options(&self, sig_proxy: bool) -> Result<Options> {
        let detach_keys = self.detach_keys.as_deref().map(DetachKeys::parse);
        Ok(Options {
            detach_keys: detach_keys.transpose()?,
            sig_proxy,
            ..Options::default()
        })
    }
}

impl SessionFlags {
    fn options(&self) -> Result<Options> {
        Ok(Options {
            detach_keys: Some(DetachKeys::parse(&self.detach_keys)?),
            sig_proxy: self.sig_proxy,
            ..Options::default()
        })
    }
}

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Sandbox name, ID or ID prefix.
    pub reference: String,
    /// Do not send this terminal's input to the process.
    #[arg(long)]
    pub no_stdin: bool,
    #[command(flatten)]
    pub flags: SessionFlags,
}

/// Attaches to a running sandbox and returns the code to exit with.
pub async fn run(socket: &str, token_store: &Path, args: Args) -> Result<i32> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, &args.reference).await?;
    if target.status != SandboxStatus::Running {
        bail!(NOT_RUNNING);
    }
    attach_main(&mut c, &target, !args.no_stdin, &args.flags, false).await
}

/// Streams the main process of `target` to the terminal. `show_history`
/// prints the output the process wrote before the attach too.
pub async fn attach_main(
    c: &mut WardClient<Channel>,
    target: &Resolved,
    forward_stdin: bool,
    flags: &SessionFlags,
    show_history: bool,
) -> Result<i32> {
    require_terminal(target.tty, forward_stdin)?;
    let options = flags.options()?;
    let start = AttachStart {
        sandbox_id: target.id.clone(),
        mode: AttachMode::MainProcess as i32,
        tty: target.tty,
        ..Default::default()
    };
    let mut live = show_history;
    let sink = |output: Output| match output {
        Output::Data(chunk) if live => attach::write_to_terminal(chunk.stream, chunk.bytes),
        Output::Data(_) => Ok(()),
        Output::ReplayEnd => {
            live = true;
            Ok(())
        }
    };
    let outcome = attach::attach_opts(c, target, start, forward_stdin, &options, sink).await?;
    require_exit(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        flags: SessionFlags,
    }

    fn flags(args: &[&str]) -> SessionFlags {
        Wrapper::try_parse_from(["attach"].iter().chain(args))
            .expect("parse")
            .flags
    }

    #[test]
    fn given_no_flags_when_parsed_then_docker_defaults_apply() {
        let parsed = flags(&[]);

        assert_eq!(parsed.detach_keys, DEFAULT_SEQUENCE);
        assert!(parsed.sig_proxy);
    }

    #[rstest::rstest]
    #[case::bare(&["--sig-proxy"], true)]
    #[case::explicit_true(&["--sig-proxy=true"], true)]
    #[case::explicit_false(&["--sig-proxy=false"], false)]
    fn given_sig_proxy_form_when_parsed_then_value_follows(
        #[case] args: &[&str],
        #[case] want: bool,
    ) {
        assert_eq!(flags(args).sig_proxy, want);
    }

    #[test]
    fn given_custom_detach_keys_when_options_built_then_they_are_used() {
        let options = flags(&["--detach-keys", "ctrl-x"]).options().unwrap();

        assert_eq!(
            options.detach_keys,
            Some(DetachKeys::parse("ctrl-x").unwrap())
        );
    }

    #[test]
    fn given_no_detach_flag_when_options_built_then_no_keys_are_active() {
        let options = DetachFlag::default().options(true).unwrap();

        assert_eq!(options.detach_keys, None);
        assert!(options.sig_proxy);
    }

    #[test]
    fn given_a_detach_flag_when_options_built_then_the_keys_and_sig_proxy_follow() {
        let flag = DetachFlag {
            detach_keys: Some("ctrl-x".into()),
        };

        let options = flag.options(false).unwrap();

        assert_eq!(
            options.detach_keys,
            Some(DetachKeys::parse("ctrl-x").unwrap())
        );
        assert!(!options.sig_proxy);
    }

    #[test]
    fn given_a_bad_detach_flag_when_options_built_then_it_fails() {
        let flag = DetachFlag {
            detach_keys: Some("nope".into()),
        };

        assert!(flag.options(true).is_err());
    }

    #[test]
    fn given_bad_detach_keys_when_options_built_then_it_fails() {
        assert!(flags(&["--detach-keys", "nope"]).options().is_err());
    }
}
