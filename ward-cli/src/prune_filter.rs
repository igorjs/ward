// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The `--filter` values of the `prune` commands: `until=`, `label=`,
//! `label!=` and, for images, `dangling=`.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};

use crate::log_output::parse_since;

const FILTER_UNTIL: &str = "until";
const FILTER_LABEL: &str = "label";
const FILTER_LABEL_NOT: &str = "label!";
const FILTER_DANGLING: &str = "dangling";

/// One `label=` or `label!=` test.
#[derive(Debug, PartialEq)]
struct LabelTest {
    key: String,
    value: Option<String>,
    /// `label!=`: the label must be absent, or have another value.
    negated: bool,
}

impl LabelTest {
    fn parse(spec: &str, negated: bool) -> Result<Self> {
        if spec.is_empty() || spec.starts_with('=') {
            bail!("invalid filter 'label={spec}'");
        }
        let (key, value) = match spec.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (spec, None),
        };
        Ok(Self {
            key: key.to_string(),
            value,
            negated,
        })
    }

    fn allows(&self, labels: &HashMap<String, String>) -> bool {
        let found = match (&self.value, labels.get(&self.key)) {
            (Some(wanted), Some(have)) => wanted == have,
            (None, have) => have.is_some(),
            (Some(_), None) => false,
        };
        found != self.negated
    }
}

/// Parsed `--filter` values. An object must pass every one of them.
#[derive(Debug, Default)]
pub struct Filter {
    until: Option<DateTime<Utc>>,
    labels: Vec<LabelTest>,
    /// `dangling=false` widens an image prune to every unused image.
    dangling: Option<bool>,
}

impl Filter {
    /// # Errors
    ///
    /// Fails on a value that is not `key=value`, an unknown key, or a value
    /// the key does not take. `dangling` is accepted only when `images` is set.
    pub fn parse(specs: &[String], now: DateTime<Utc>, images: bool) -> Result<Self> {
        let mut filter = Self::default();
        for spec in specs {
            let Some((key, value)) = spec.split_once('=') else {
                bail!("bad format of filter (expected name=value): {spec}");
            };
            match key {
                FILTER_UNTIL => {
                    filter.until = Some(parse_since(value, now).map_err(|_| {
                        anyhow!(
                            "invalid filter 'until={value}': use a duration like 10m, a timestamp like 2026-01-02T03:04:05Z, or Unix seconds"
                        )
                    })?);
                }
                FILTER_LABEL => filter.labels.push(LabelTest::parse(value, false)?),
                FILTER_LABEL_NOT => filter.labels.push(LabelTest::parse(value, true)?),
                FILTER_DANGLING if images => {
                    filter.dangling = Some(match value {
                        "true" => true,
                        "false" => false,
                        _ => bail!("invalid filter 'dangling={value}'"),
                    });
                }
                other => bail!("invalid filter '{other}'"),
            }
        }
        Ok(filter)
    }

    /// Whether `dangling=false` asked for every unused image.
    pub fn wants_all_images(&self) -> bool {
        self.dangling == Some(false)
    }

    /// Whether an object created at `created` with `labels` passes. An object
    /// with no creation time passes `until`, since its age is unknown.
    pub fn allows(&self, created: Option<i64>, labels: &HashMap<String, String>) -> bool {
        let old_enough = match (self.until, created) {
            (Some(until), Some(seconds)) => seconds < until.timestamp(),
            _ => true,
        };
        old_enough && self.labels.iter().all(|test| test.allows(labels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_SECONDS: i64 = 1_000_000;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW_SECONDS, 0).expect("time")
    }

    fn filter(specs: &[&str]) -> Filter {
        let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
        Filter::parse(&specs, now(), true).expect("parse")
    }

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[rstest::rstest]
    #[case::older(&["until=10m"], Some(NOW_SECONDS - 601), &[], true)]
    #[case::newer(&["until=10m"], Some(NOW_SECONDS - 599), &[], false)]
    #[case::unknown_age(&["until=10m"], None, &[], true)]
    #[case::timestamp(&["until=1970-01-01T00:00:10Z"], Some(9), &[], true)]
    #[case::label_present(&["label=env"], Some(0), &[("env", "x")], true)]
    #[case::label_absent(&["label=env"], Some(0), &[], false)]
    #[case::label_value(&["label=env=prod"], Some(0), &[("env", "prod")], true)]
    #[case::label_other_value(&["label=env=prod"], Some(0), &[("env", "dev")], false)]
    #[case::negated_present(&["label!=env"], Some(0), &[("env", "x")], false)]
    #[case::negated_absent(&["label!=env"], Some(0), &[], true)]
    #[case::negated_value(&["label!=env=prod"], Some(0), &[("env", "dev")], true)]
    #[case::all_must_pass(&["label=a", "label=b"], Some(0), &[("a", "1")], false)]
    fn given_a_filter_when_checked_then_every_clause_must_pass(
        #[case] specs: &[&str],
        #[case] created: Option<i64>,
        #[case] have: &[(&str, &str)],
        #[case] expected: bool,
    ) {
        assert_eq!(filter(specs).allows(created, &labels(have)), expected);
    }

    #[rstest::rstest]
    #[case::no_equals("until")]
    #[case::unknown_key("colour=red")]
    #[case::bad_until("until=soon")]
    #[case::empty_label("label=")]
    #[case::bad_dangling("dangling=maybe")]
    fn given_a_bad_filter_when_parsed_then_it_is_refused(#[case] spec: &str) {
        assert!(Filter::parse(&[spec.to_string()], now(), true).is_err());
    }

    #[test]
    fn given_dangling_false_when_parsed_then_every_unused_image_is_wanted() {
        assert!(filter(&["dangling=false"]).wants_all_images());
        assert!(!filter(&["dangling=true"]).wants_all_images());
    }

    #[test]
    fn given_dangling_outside_image_prune_when_parsed_then_it_is_refused() {
        let result = Filter::parse(&["dangling=true".to_string()], now(), false);

        assert!(result.is_err());
    }
}
