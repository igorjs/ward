// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Which sandboxes `ward ps` shows: `--filter`, `-n` and `-l`, newest first.

use anyhow::{Result, bail};
use ward_core::pb::{SandboxInfo, SandboxStatus};

const FILTER_NAME: &str = "name";
const FILTER_ID: &str = "id";
const FILTER_STATUS: &str = "status";
const FILTER_LABEL: &str = "label";
/// Docker's name for a sandbox that exists but has not started.
const STATUS_CREATED: &str = "created";
const STATUS_PAUSED: &str = "paused";
const STATUSES: [&str; 7] = [
    STATUS_CREATED,
    "running",
    STATUS_PAUSED,
    "stopping",
    "stopped",
    "exited",
    "failed",
];
/// `-n` value that means no limit.
pub const NO_LIMIT: i64 = -1;
/// What `-l` stands for when `-n` is not given.
const LATEST_COUNT: usize = 1;

/// Parsed `--filter` values. A sandbox must match one value of every key given.
#[derive(Debug, Default)]
pub struct Filters {
    names: Vec<String>,
    ids: Vec<String>,
    statuses: Vec<String>,
    /// `key` or `key=value`; a sandbox must match every one.
    labels: Vec<String>,
}

impl Filters {
    /// # Errors
    ///
    /// Fails on a filter that is not `key=value`, an unknown key or status.
    pub fn parse(specs: &[String]) -> Result<Self> {
        let mut filters = Self::default();
        for spec in specs {
            let Some((key, value)) = spec.split_once('=') else {
                bail!("bad format of filter (expected name=value): {spec}");
            };
            match key {
                FILTER_NAME => filters.names.push(value.to_string()),
                FILTER_ID => filters.ids.push(value.to_string()),
                FILTER_STATUS if STATUSES.contains(&value) => {
                    filters.statuses.push(value.to_string());
                }
                FILTER_STATUS => bail!("invalid filter 'status={value}'"),
                FILTER_LABEL if value.is_empty() || value.starts_with('=') => {
                    bail!("invalid filter 'label={value}'")
                }
                FILTER_LABEL => filters.labels.push(value.to_string()),
                _ => bail!("invalid filter '{key}'"),
            }
        }
        Ok(filters)
    }

    fn allows(&self, info: &SandboxInfo) -> bool {
        let any = |wanted: &[String], matches: &dyn Fn(&str) -> bool| {
            wanted.is_empty() || wanted.iter().any(|w| matches(w))
        };
        any(&self.names, &|w| info.name.contains(w))
            && any(&self.ids, &|w| info.id.starts_with(w))
            && any(&self.statuses, &|w| w == status_word(info))
            && self.labels.iter().all(|label| match label.split_once('=') {
                Some((key, value)) => info.labels.get(key).is_some_and(|v| v == value),
                None => info.labels.contains_key(label),
            })
    }
}

fn status_word(info: &SandboxInfo) -> &'static str {
    if info.paused {
        return STATUS_PAUSED;
    }
    match SandboxStatus::try_from(info.status).unwrap_or(SandboxStatus::Unspecified) {
        SandboxStatus::Creating => STATUS_CREATED,
        _ => crate::status_name(info.status),
    }
}

/// How many sandboxes `-n` and `-l` ask for. `-n` wins over `-l`.
pub fn limit(last: i64, latest: bool) -> Option<usize> {
    match (usize::try_from(last), latest) {
        (Ok(count), _) if count > 0 => Some(count),
        (_, true) => Some(LATEST_COUNT),
        _ => None,
    }
}

/// The sandboxes to show, newest first. Like Docker, `-n`, `-l` and a status
/// filter look at every state.
pub fn select<'a>(
    listed: &'a [SandboxInfo],
    all: bool,
    filters: &Filters,
    limit: Option<usize>,
) -> Vec<&'a SandboxInfo> {
    let every_state = all || limit.is_some() || !filters.statuses.is_empty();
    let mut shown: Vec<&SandboxInfo> = listed
        .iter()
        .filter(|s| every_state || s.status == SandboxStatus::Running as i32)
        .filter(|s| filters.allows(s))
        .collect();
    let created = |s: &SandboxInfo| s.created_at.map(|t| (t.seconds, t.nanos));
    shown.sort_by(|a, b| created(b).cmp(&created(a)).then_with(|| a.id.cmp(&b.id)));
    if let Some(limit) = limit {
        shown.truncate(limit);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(id: &str, name: &str, status: SandboxStatus, created: i64) -> SandboxInfo {
        SandboxInfo {
            id: id.into(),
            name: name.into(),
            status: status as i32,
            created_at: Some(prost_types::Timestamp {
                seconds: created,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    fn fleet() -> Vec<SandboxInfo> {
        vec![
            sandbox("aaa1", "web1", SandboxStatus::Running, 10),
            sandbox("bbb2", "web2", SandboxStatus::Exited, 30),
            sandbox("ccc3", "db", SandboxStatus::Creating, 20),
        ]
    }

    fn names(shown: &[&SandboxInfo]) -> Vec<String> {
        shown.iter().map(|s| s.name.clone()).collect()
    }

    fn parse(specs: &[&str]) -> Filters {
        let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
        Filters::parse(&specs).expect("parse")
    }

    #[test]
    fn given_no_flags_when_selected_then_only_running_are_shown() {
        let listed = fleet();

        let shown = select(&listed, false, &Filters::default(), None);

        assert_eq!(names(&shown), ["web1"]);
    }

    #[test]
    fn given_all_when_selected_then_every_state_is_listed_newest_first() {
        let listed = fleet();

        let shown = select(&listed, true, &Filters::default(), None);

        assert_eq!(names(&shown), ["web2", "db", "web1"]);
    }

    #[rstest::rstest]
    #[case::substring_name(&["name=web"], &["web2", "web1"])]
    #[case::id_prefix(&["id=ccc"], &["db"])]
    #[case::created_status(&["status=created"], &["db"])]
    #[case::same_key_is_or(&["name=db", "name=web1"], &["db", "web1"])]
    #[case::keys_are_and(&["name=web", "status=running"], &["web1"])]
    fn given_filters_when_selected_then_matching_sandboxes_remain(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let listed = fleet();

        let shown = select(&listed, true, &parse(specs), None);

        assert_eq!(names(&shown), expected);
    }

    #[rstest::rstest]
    #[case::key_only(&["label=team"], &["web1", "db"])]
    #[case::key_and_value(&["label=team=infra"], &["web1"])]
    #[case::wrong_value(&["label=team=none"], &[])]
    #[case::all_must_match(&["label=team", "label=tier=1"], &["db"])]
    fn given_label_filters_when_selected_then_every_label_must_match(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let mut listed = fleet();
        let label = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        listed[0].labels = label(&[("team", "infra")]);
        listed[2].labels = label(&[("team", "data"), ("tier", "1")]);

        let shown = select(&listed, true, &parse(specs), None);

        let mut got = names(&shown);
        got.sort();
        let mut want: Vec<String> = expected.iter().map(ToString::to_string).collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn given_paused_sandbox_when_filtered_then_paused_matches_and_running_does_not() {
        let mut listed = fleet();
        let web1 = listed.iter_mut().find(|s| s.name == "web1").expect("web1");
        web1.paused = true;

        let paused = select(&listed, true, &parse(&["status=paused"]), None);
        let running = select(&listed, true, &parse(&["status=running"]), None);

        assert_eq!(names(&paused), ["web1"]);
        assert!(!names(&running).contains(&"web1".to_string()));
    }

    #[test]
    fn given_status_filter_without_all_when_selected_then_that_state_is_still_listed() {
        let listed = fleet();

        let shown = select(&listed, false, &parse(&["status=exited"]), None);

        assert_eq!(names(&shown), ["web2"]);
    }

    #[rstest::rstest]
    #[case::unknown_key("color=red", "invalid filter 'color'")]
    #[case::bad_status("status=sleeping", "status=sleeping")]
    #[case::empty_label("label=", "label=")]
    #[case::no_value("name", "name=value")]
    fn given_bad_filter_when_parsed_then_error_says_why(#[case] spec: &str, #[case] said: &str) {
        let error = Filters::parse(&[spec.to_string()]).expect_err("rejected");

        assert!(error.to_string().contains(said), "{error}");
    }

    #[rstest::rstest]
    #[case::default(NO_LIMIT, false, None)]
    #[case::latest(NO_LIMIT, true, Some(1))]
    #[case::last(2, false, Some(2))]
    #[case::last_beats_latest(2, true, Some(2))]
    fn given_n_and_l_when_limit_resolved_then_n_wins(
        #[case] last: i64,
        #[case] latest: bool,
        #[case] expected: Option<usize>,
    ) {
        assert_eq!(limit(last, latest), expected);
    }

    #[test]
    fn given_limit_when_selected_then_newest_n_of_every_state_remain() {
        let listed = fleet();

        let shown = select(&listed, false, &Filters::default(), Some(2));

        assert_eq!(names(&shown), ["web2", "db"]);
    }
}
