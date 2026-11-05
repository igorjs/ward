// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Which sandboxes `ward ps` shows: `--filter`, `-n` and `-l`, newest first.

use anyhow::{Result, bail};
use ward_core::pb::{ImageInfo, PortMapping, SandboxInfo, SandboxStatus};

use crate::exists::names_image;
use crate::ps_columns::uses_image;

const FILTER_NAME: &str = "name";
const FILTER_ID: &str = "id";
const FILTER_STATUS: &str = "status";
const FILTER_LABEL: &str = "label";
const FILTER_HEALTH: &str = "health";
const FILTER_ANCESTOR: &str = "ancestor";
const FILTER_BEFORE: &str = "before";
const FILTER_SINCE: &str = "since";
const FILTER_EXITED: &str = "exited";
const FILTER_PUBLISH: &str = "publish";
const FILTER_EXPOSE: &str = "expose";
const FILTER_VOLUME: &str = "volume";
const PORT_RANGE_SEPARATOR: char = '-';
const PROTOCOL_SEPARATOR: char = '/';
/// The only protocol a published port can have.
const PROTOCOL_TCP: &str = "tcp";
const DEFAULT_TAG_SUFFIX: &str = ":latest";
/// Docker's word for a sandbox with no health check.
const HEALTH_NONE: &str = "none";
const HEALTHS: [&str; 4] = ["starting", "healthy", "unhealthy", HEALTH_NONE];
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
    healths: Vec<String>,
    /// `key` or `key=value`; a sandbox must match every one.
    labels: Vec<String>,
    ancestors: Vec<Ancestor>,
    exit_codes: Vec<i32>,
    /// Name or ID of the sandbox the others must predate; the last one given wins.
    before: Option<String>,
    /// Name or ID of the sandbox the others must follow; the last one given wins.
    since: Option<String>,
    before_at: Option<(i64, i32)>,
    since_at: Option<(i64, i32)>,
    publishes: Vec<PortSpec>,
    exposes: Vec<PortSpec>,
    /// A volume name, or a path inside or outside the guest that is mounted.
    volumes: Vec<String>,
}

/// An `ancestor=` value, tied to the cached image it names when there is one.
#[derive(Debug)]
struct Ancestor {
    wanted: String,
    image: Option<ImageInfo>,
}

impl Ancestor {
    fn matches(&self, info: &SandboxInfo) -> bool {
        match &self.image {
            Some(image) => uses_image(info, image),
            None => with_tag(&info.image) == with_tag(&self.wanted),
        }
    }
}

fn with_tag(reference: &str) -> String {
    match reference.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => reference.to_string(),
        _ => format!("{reference}{DEFAULT_TAG_SUFFIX}"),
    }
}

/// A `publish=` or `expose=` value: `80`, `80/tcp`, `8000-8010` or
/// `8000-8010/tcp`.
#[derive(Debug)]
struct PortSpec {
    first: u32,
    last: u32,
}

impl PortSpec {
    fn parse(filter: &str, value: &str) -> Result<Self> {
        let bad = || anyhow::anyhow!("invalid filter '{filter}={value}'");
        let (ports, protocol) = match value.split_once(PROTOCOL_SEPARATOR) {
            Some((ports, protocol)) => (ports, protocol),
            None => (value, PROTOCOL_TCP),
        };
        let (first, last) = match ports.split_once(PORT_RANGE_SEPARATOR) {
            Some((first, last)) => (first, last),
            None => (ports, ports),
        };
        let (first, last) = (
            first.parse().map_err(|_| bad())?,
            last.parse().map_err(|_| bad())?,
        );
        if first > last || protocol != PROTOCOL_TCP {
            return Err(bad());
        }
        Ok(Self { first, last })
    }

    fn matches(&self, mapping: &PortMapping) -> bool {
        (self.first..=self.last).contains(&mapping.container_port)
    }
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
                FILTER_HEALTH if HEALTHS.contains(&value) => {
                    filters.healths.push(value.to_string());
                }
                FILTER_HEALTH => bail!("invalid filter 'health={value}'"),
                FILTER_LABEL if value.is_empty() || value.starts_with('=') => {
                    bail!("invalid filter 'label={value}'")
                }
                FILTER_LABEL => filters.labels.push(value.to_string()),
                FILTER_ANCESTOR if !value.is_empty() => filters.ancestors.push(Ancestor {
                    wanted: value.to_string(),
                    image: None,
                }),
                FILTER_BEFORE if !value.is_empty() => filters.before = Some(value.to_string()),
                FILTER_SINCE if !value.is_empty() => filters.since = Some(value.to_string()),
                FILTER_EXITED => match value.parse() {
                    Ok(code) => filters.exit_codes.push(code),
                    Err(_) => bail!("invalid filter 'exited={value}'"),
                },
                FILTER_PUBLISH => filters.publishes.push(PortSpec::parse(key, value)?),
                FILTER_EXPOSE => filters.exposes.push(PortSpec::parse(key, value)?),
                FILTER_VOLUME if !value.is_empty() => filters.volumes.push(value.to_string()),
                FILTER_ANCESTOR | FILTER_BEFORE | FILTER_SINCE | FILTER_VOLUME => {
                    bail!("invalid filter '{key}={value}'")
                }
                _ => bail!("invalid filter '{key}'"),
            }
        }
        Ok(filters)
    }

    /// Whether the filters need the cached images to be resolved.
    pub fn needs_images(&self) -> bool {
        !self.ancestors.is_empty()
    }

    /// Looks up what `ancestor=`, `before=` and `since=` name.
    ///
    /// # Errors
    ///
    /// Fails when `before=` or `since=` names no sandbox.
    pub fn resolve(&mut self, listed: &[SandboxInfo], images: &[ImageInfo]) -> Result<()> {
        for ancestor in &mut self.ancestors {
            ancestor.image = images
                .iter()
                .find(|image| names_image(image, &ancestor.wanted))
                .cloned();
        }
        let created_of = |name: &Option<String>| -> Result<Option<(i64, i32)>> {
            let Some(name) = name else { return Ok(None) };
            match listed
                .iter()
                .find(|s| s.name == *name || s.id.starts_with(name.as_str()))
            {
                Some(found) => Ok(created(found)),
                None => bail!("No such container: {name}"),
            }
        };
        self.before_at = created_of(&self.before)?;
        self.since_at = created_of(&self.since)?;
        Ok(())
    }

    fn allows(&self, info: &SandboxInfo) -> bool {
        let any = |wanted: &[String], matches: &dyn Fn(&str) -> bool| {
            wanted.is_empty() || wanted.iter().any(|w| matches(w))
        };
        any(&self.names, &|w| info.name.contains(w))
            && any(&self.ids, &|w| info.id.starts_with(w))
            && any(&self.statuses, &|w| w == status_word(info))
            && any(&self.healths, &|w| {
                w == info
                    .health
                    .as_ref()
                    .map_or(HEALTH_NONE, |h| h.status.as_str())
            })
            && (self.ancestors.is_empty() || self.ancestors.iter().any(|a| a.matches(info)))
            && (self.exit_codes.is_empty()
                || info
                    .exit_code
                    .is_some_and(|code| self.exit_codes.contains(&code)))
            && self
                .before_at
                .is_none_or(|cutoff| created(info) < Some(cutoff))
            && self
                .since_at
                .is_none_or(|cutoff| created(info) > Some(cutoff))
            && (self.publishes.is_empty() || self.publishes.iter().any(|p| publishes(info, p)))
            && (self.exposes.is_empty() || self.exposes.iter().any(|p| publishes(info, p)))
            && (self.volumes.is_empty()
                || self.volumes.iter().any(|wanted| {
                    info.mounts
                        .iter()
                        .any(|m| m.target == *wanted || m.source == *wanted)
                }))
            && self.labels.iter().all(|label| match label.split_once('=') {
                Some((key, value)) => info.labels.get(key).is_some_and(|v| v == value),
                None => info.labels.contains_key(label),
            })
    }
}

/// Whether the sandbox publishes a container port in the range. Ward has no
/// image `EXPOSE`, so a port is exposed exactly when it is published.
fn publishes(info: &SandboxInfo, ports: &PortSpec) -> bool {
    info.published_ports.iter().any(|m| ports.matches(m))
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
    shown.sort_by(|a, b| created(b).cmp(&created(a)).then_with(|| a.id.cmp(&b.id)));
    if let Some(limit) = limit {
        shown.truncate(limit);
    }
    shown
}

fn created(info: &SandboxInfo) -> Option<(i64, i32)> {
    info.created_at.map(|t| (t.seconds, t.nanos))
}

/// A column `ps --sort` orders by, as Podman names them. Podman's `size` is
/// left out because Ward does not measure it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SortField {
    Command,
    Created,
    Id,
    Image,
    Names,
    Runningfor,
    Status,
}

/// Orders `shown` by `field`, ascending: oldest first for `created`, newest
/// first for `runningfor`. Ties fall back to the ID.
pub fn sort(shown: &mut [&SandboxInfo], field: SortField) {
    shown.sort_by(|a, b| {
        let order = match field {
            SortField::Command => a.command_name.cmp(&b.command_name),
            SortField::Created => created(a).cmp(&created(b)),
            SortField::Id => a.id.cmp(&b.id),
            SortField::Image => a.image.cmp(&b.image),
            SortField::Names => a.name.cmp(&b.name),
            SortField::Runningfor => created(b).cmp(&created(a)),
            SortField::Status => status_word(a).cmp(status_word(b)),
        };
        order.then_with(|| a.id.cmp(&b.id))
    });
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
    #[case::created(SortField::Created, &["web1", "db", "web2"])]
    #[case::runningfor(SortField::Runningfor, &["web2", "db", "web1"])]
    #[case::names(SortField::Names, &["db", "web1", "web2"])]
    #[case::id(SortField::Id, &["web1", "web2", "db"])]
    #[case::status(SortField::Status, &["db", "web2", "web1"])]
    fn given_a_sort_field_when_sorted_then_the_order_is_ascending(
        #[case] field: SortField,
        #[case] expected: &[&str],
    ) {
        let listed = fleet();
        let mut shown = select(&listed, true, &Filters::default(), None);

        sort(&mut shown, field);

        assert_eq!(names(&shown), expected);
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

    #[rstest::rstest]
    #[case::healthy("health=healthy", &["ok"])]
    #[case::unhealthy("health=unhealthy", &["sick"])]
    #[case::none("health=none", &["plain"])]
    fn given_a_health_filter_when_selected_then_only_that_health_remains(
        #[case] spec: &str,
        #[case] want: &[&str],
    ) {
        // Arrange
        let with_health = |name: &str, status: Option<&str>| SandboxInfo {
            health: status.map(|status| ward_core::pb::HealthState {
                status: status.into(),
                ..Default::default()
            }),
            ..sandbox(name, name, SandboxStatus::Running, 1)
        };
        let fleet = [
            with_health("ok", Some("healthy")),
            with_health("sick", Some("unhealthy")),
            with_health("plain", None),
        ];

        // Act
        let shown = select(&fleet, false, &parse(&[spec]), None);

        // Assert
        assert_eq!(names(&shown), want);
        assert!(Filters::parse(&["health=sleepy".to_string()]).is_err());
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

    fn resolved(specs: &[&str], images: &[ImageInfo]) -> Filters {
        let mut filters = parse(specs);
        filters.resolve(&fleet(), images).expect("resolve");
        filters
    }

    fn with_ports(mut info: SandboxInfo, ports: &[u32]) -> SandboxInfo {
        info.published_ports = ports
            .iter()
            .map(|&container_port| PortMapping {
                container_port,
                host_port: 8080,
                ..Default::default()
            })
            .collect();
        info
    }

    #[rstest::rstest]
    #[case::older_than(&["before=web2"], &["db", "web1"])]
    #[case::newer_than(&["since=web1"], &["web2", "db"])]
    #[case::by_id_prefix(&["since=bbb"], &[])]
    #[case::window(&["since=web1", "before=web2"], &["db"])]
    fn given_before_or_since_when_selected_then_only_that_side_of_the_sandbox_remains(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let filters = resolved(specs, &[]);

        let listed = fleet();
        let shown = select(&listed, true, &filters, None);

        assert_eq!(names(&shown), expected);
    }

    #[test]
    fn given_before_naming_nothing_when_resolved_then_it_is_refused() {
        let mut filters = parse(&["before=ghost"]);

        let error = filters.resolve(&fleet(), &[]).expect_err("refused");

        assert_eq!(error.to_string(), "No such container: ghost");
    }

    #[rstest::rstest]
    #[case::code_zero(&["exited=0"], &["web2"])]
    #[case::other_code(&["exited=3"], &[])]
    fn given_exited_when_selected_then_the_exit_code_decides(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let mut listed = fleet();
        listed[1].exit_code = Some(0);

        let shown = select(&listed, true, &parse(specs), None);

        assert_eq!(names(&shown), expected);
    }

    #[test]
    fn given_a_cached_image_when_ancestor_resolved_then_a_tag_or_id_prefix_finds_its_sandboxes() {
        let image = ImageInfo {
            reference: "alpine:3".into(),
            digest: "sha256:abcdef0123456789".into(),
            ..Default::default()
        };
        let mut listed = fleet();
        listed[0].image = "alpine:3".into();
        listed[1].image = "busybox".into();
        let mut by_tag = parse(&["ancestor=alpine:3"]);
        let mut by_id = parse(&["ancestor=abcdef"]);
        let mut uncached = parse(&["ancestor=busybox"]);
        for filters in [&mut by_tag, &mut by_id, &mut uncached] {
            filters
                .resolve(&listed, std::slice::from_ref(&image))
                .expect("resolve");
        }

        let tag = names(&select(&listed, true, &by_tag, None));
        let id = names(&select(&listed, true, &by_id, None));
        let plain = names(&select(&listed, true, &uncached, None));

        assert_eq!(tag, ["web1"]);
        assert_eq!(id, ["web1"]);
        assert_eq!(plain, ["web2"]);
    }

    #[rstest::rstest]
    #[case::single(&["publish=80"], &["web1"])]
    #[case::with_protocol(&["publish=80/tcp"], &["web1"])]
    #[case::range(&["publish=8000-8100"], &["web2"])]
    #[case::either(&["publish=80", "publish=8080"], &["web2", "web1"])]
    #[case::unpublished(&["publish=22"], &[])]
    #[case::expose_matches_published(&["expose=80"], &["web1"])]
    fn given_port_filters_when_selected_then_published_container_ports_decide(
        #[case] specs: &[&str],
        #[case] expected: &[&str],
    ) {
        let mut listed = fleet();
        listed[0] = with_ports(listed[0].clone(), &[80]);
        listed[1] = with_ports(listed[1].clone(), &[8080]);

        let shown = select(&listed, true, &parse(specs), None);

        assert_eq!(names(&shown), expected);
    }

    #[test]
    fn given_volume_filter_when_selected_then_a_mount_source_or_target_matches() {
        let mut listed = fleet();
        listed[0].mounts = vec![ward_core::pb::Mount {
            source: "/srv/data".into(),
            target: "/data".into(),
            readonly: false,
        }];

        let by_target = select(&listed, true, &parse(&["volume=/data"]), None);
        let by_source = select(&listed, true, &parse(&["volume=/srv/data"]), None);
        let miss = select(&listed, true, &parse(&["volume=/other"]), None);

        assert_eq!(names(&by_target), ["web1"]);
        assert_eq!(names(&by_source), ["web1"]);
        assert!(miss.is_empty());
    }

    #[rstest::rstest]
    #[case::unknown_key("color=red", "invalid filter 'color'")]
    #[case::bad_exit("exited=x", "exited=x")]
    #[case::bad_port("publish=http", "publish=http")]
    #[case::udp("expose=53/udp", "expose=53/udp")]
    #[case::backwards_range("publish=90-80", "publish=90-80")]
    #[case::empty_ancestor("ancestor=", "ancestor=")]
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
