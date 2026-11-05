// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The `-t` and `-s` of `ward stop` and `ward restart`.

use anyhow::Result;

/// How a stop or restart ends the main process.
#[derive(Debug, Default, Clone)]
pub struct StopOptions {
    /// Seconds to wait after the stop signal before SIGKILL.
    pub grace_seconds: Option<i32>,
    /// Signal to send instead of the sandbox's stop signal.
    pub signal: Option<String>,
}

impl StopOptions {
    /// Check the signal before anything is stopped.
    ///
    /// # Errors
    ///
    /// Fails when the signal is not a name or number Ward knows.
    pub fn new(grace_seconds: Option<i32>, signal: Option<String>) -> Result<Self> {
        if let Some(signal) = &signal {
            crate::inspect::parse_signal(signal)
                .map_err(|_| anyhow::anyhow!("invalid stop signal: {signal}"))?;
        }
        Ok(Self {
            grace_seconds,
            signal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_known_signal_when_options_built_then_it_is_kept() {
        let options = StopOptions::new(Some(5), Some("SIGINT".into())).unwrap();

        assert_eq!(options.grace_seconds, Some(5));
        assert_eq!(options.signal.as_deref(), Some("SIGINT"));
    }

    #[test]
    fn given_no_signal_when_options_built_then_the_sandbox_keeps_its_own() {
        let options = StopOptions::new(None, None).unwrap();

        assert_eq!(options.signal, None);
    }

    #[test]
    fn given_an_unknown_signal_when_options_built_then_it_fails_before_any_stop() {
        let error = StopOptions::new(None, Some("SIGNOPE".into())).unwrap_err();

        assert_eq!(error.to_string(), "invalid stop signal: SIGNOPE");
    }
}
