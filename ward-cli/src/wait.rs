// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward wait SANDBOX...`: block until each sandbox's main process ends and
//! print its exit code, one per line like `docker wait`.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use ward_core::pb::{WaitCondition, WaitRequest};

use crate::client;
use crate::lifecycle::Failures;

/// What to wait for, as Docker's `--condition` names it.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Condition {
    /// The sandbox is not running.
    #[default]
    NotRunning,
    /// The sandbox's main process exits next.
    NextExit,
    /// The sandbox is removed.
    Removed,
}

impl From<Condition> for WaitCondition {
    fn from(condition: Condition) -> Self {
        match condition {
            Condition::NotRunning => Self::NotRunning,
            Condition::NextExit => Self::NextExit,
            Condition::Removed => Self::Removed,
        }
    }
}

/// Waits for each reference in turn. A reference that fails goes to stderr
/// and the rest are still waited for.
pub async fn run(
    socket: &str,
    token_store: &Path,
    refs: &[String],
    condition: Condition,
    json: bool,
) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let mut failures = Failures::new();
    for reference in refs {
        let result = async {
            let target = client::resolve(&mut c, token_store, reference).await?;
            let request = target.request(WaitRequest {
                sandbox_id: target.id.clone(),
                condition: WaitCondition::from(condition) as i32,
            })?;
            Ok::<_, anyhow::Error>(c.wait(request).await?.into_inner().exit_code)
        }
        .await;
        match result {
            Ok(code) if json => println!("{}", json!({"sandbox": reference, "exit_code": code})),
            Ok(code) => println!("{code}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(refs.len(), "waited for")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[rstest::rstest]
    #[case::default("not-running", Condition::NotRunning)]
    #[case::next("next-exit", Condition::NextExit)]
    #[case::removed("removed", Condition::Removed)]
    fn given_docker_condition_name_when_parsed_then_it_maps(
        #[case] name: &str,
        #[case] want: Condition,
    ) {
        assert_eq!(Condition::from_str(name, false).unwrap(), want);
    }

    #[test]
    fn given_unknown_condition_when_parsed_then_it_fails() {
        assert!(Condition::from_str("later", false).is_err());
    }

    #[test]
    fn given_conditions_when_converted_then_wire_values_follow() {
        assert_eq!(
            WaitCondition::from(Condition::NextExit),
            WaitCondition::NextExit
        );
        assert_eq!(
            WaitCondition::from(Condition::default()),
            WaitCondition::NotRunning
        );
    }
}
