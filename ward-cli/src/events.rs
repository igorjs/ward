// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward events`: the daemon's sandbox events, printed as `docker events`
//! prints container events.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use ward_core::pb::{EventsRequest, SandboxEvent};

use crate::client;
use crate::log_output::parse_since;
use crate::template::Template;

/// Sandboxes are the only objects the daemon reports on.
const EVENT_TYPE: &str = "container";
const EVENT_SCOPE: &str = "local";
/// Ends a filter key to refuse the value: `label!=env=dev`.
const NEGATION_MARK: char = '!';
const FILTER_TYPE: &str = "type";
const FILTER_EVENT: &str = "event";
const FILTER_CONTAINER: &str = "container";
const FILTER_NAME: &str = "name";
const FILTER_IMAGE: &str = "image";
const FILTER_LABEL: &str = "label";
const FILTER_REASON: &str = "reason";
const REASON_ATTRIBUTE: &str = "reason";
const IMAGE_ATTRIBUTE: &str = "image";
const NAME_ATTRIBUTE: &str = "name";
const JSON_FORMAT: &str = "{{json .}}";
const JSON_SHORTHAND: &str = "json";
/// Keys `{{.Name}}` may use in `--format`.
const FIELDS: [(&str, &str); 10] = [
    ("Type", "TYPE"),
    ("Action", "ACTION"),
    ("Status", "STATUS"),
    ("ID", "ID"),
    ("From", "FROM"),
    ("Scope", "SCOPE"),
    ("Time", "TIME"),
    ("TimeNano", "TIMENANO"),
    ("Actor.ID", "ACTOR ID"),
    (ATTRIBUTES_FIELD, "ATTRIBUTE"),
];
/// Prefix of the template fields that read one event attribute.
const ATTRIBUTES_FIELD: &str = "Actor.Attributes.";

pub struct Options {
    pub since: Option<String>,
    pub until: Option<String>,
    pub filters: Vec<String>,
    pub format: Option<String>,
    pub json: bool,
}

/// Streams events until `--until`, or until interrupted.
pub async fn run(socket: &str, options: Options) -> Result<()> {
    let filters = Filters::parse(&options.filters)?;
    let format = Format::choose(options.format.as_deref(), options.json)?;
    let now = Utc::now();
    let request = EventsRequest {
        since: options
            .since
            .as_deref()
            .map(|v| timestamp(v, now))
            .transpose()?,
        until: options
            .until
            .as_deref()
            .map(|v| timestamp(v, now))
            .transpose()?,
    };
    let mut c = client::connect(socket).await?;
    let mut stream = c.events(request).await?.into_inner();
    while let Some(event) = stream.message().await? {
        if event.dropped > 0 {
            eprintln!(
                "Warning: {} events were dropped because they were read too slowly",
                event.dropped
            );
        }
        if filters.allows(&event) {
            print!("{}", format.render(&event)?);
        }
    }
    Ok(())
}

fn timestamp(value: &str, now: DateTime<Utc>) -> Result<prost_types::Timestamp> {
    let at = parse_since(value, now).map_err(|_| {
        anyhow!(
            "invalid value '{value}' for --since or --until: use a duration like 10m, a timestamp like 2026-01-02T03:04:05Z, or Unix seconds"
        )
    })?;
    Ok(prost_types::Timestamp {
        seconds: at.timestamp(),
        nanos: at.timestamp_subsec_nanos() as i32,
    })
}

/// Values wanted and refused for one filter key.
#[derive(Default)]
struct Wanted {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl Wanted {
    fn push(&mut self, value: &str, negated: bool) {
        let list = if negated {
            &mut self.exclude
        } else {
            &mut self.include
        };
        list.push(value.to_string());
    }

    /// True when some wanted value matches (or none is wanted) and no refused
    /// value matches.
    fn allows(&self, matches: &dyn Fn(&str) -> bool) -> bool {
        (self.include.is_empty() || self.include.iter().any(|w| matches(w)))
            && !self.exclude.iter().any(|w| matches(w))
    }
}

/// `--filter` values: different keys all must match, values of one key are
/// alternatives, and `key!=value` refuses a value.
#[derive(Default)]
struct Filters {
    types: Wanted,
    events: Wanted,
    containers: Wanted,
    images: Wanted,
    /// Why a sandbox ended, as `die` events carry it.
    reasons: Wanted,
    /// Attribute filters: `key` or `key=value`. Events carry no labels, so
    /// an attribute is the closest match.
    labels: Wanted,
}

impl Filters {
    fn parse(filters: &[String]) -> Result<Self> {
        let mut parsed = Self::default();
        for filter in filters {
            let (key, value) = filter
                .split_once('=')
                .filter(|(key, value)| !key.is_empty() && !value.is_empty())
                .ok_or_else(|| anyhow!("bad format of filter (expected name=value)"))
                .with_context(|| {
                    format!("invalid argument {filter:?} for \"-f, --filter\" flag")
                })?;
            let (key, negated) = match key.strip_suffix(NEGATION_MARK) {
                Some(key) => (key, true),
                None => (key, false),
            };
            let wanted = match key {
                FILTER_TYPE => &mut parsed.types,
                FILTER_EVENT => &mut parsed.events,
                FILTER_CONTAINER | FILTER_NAME => &mut parsed.containers,
                FILTER_IMAGE => &mut parsed.images,
                FILTER_REASON => &mut parsed.reasons,
                FILTER_LABEL => &mut parsed.labels,
                other => bail!("Invalid filter '{other}'"),
            };
            wanted.push(value, negated);
        }
        Ok(parsed)
    }

    fn allows(&self, event: &SandboxEvent) -> bool {
        let attribute = |key: &str| event.attributes.get(key).map_or("", String::as_str);
        self.types.allows(&|t| t == EVENT_TYPE)
            && self.events.allows(&|e| e == event.action)
            && self
                .containers
                .allows(&|c| c == attribute(NAME_ATTRIBUTE) || event.sandbox_id.starts_with(c))
            && self.images.allows(&|i| i == attribute(IMAGE_ATTRIBUTE))
            && self.reasons.allows(&|r| r == attribute(REASON_ATTRIBUTE))
            && self.labels.allows(&|label| match label.split_once('=') {
                Some((key, value)) => event.attributes.get(key).is_some_and(|v| v == value),
                None => event.attributes.contains_key(label),
            })
    }
}

enum Format {
    Default,
    Json,
    Template(Template),
}

impl Format {
    fn choose(format: Option<&str>, json: bool) -> Result<Self> {
        Ok(match format {
            Some(JSON_SHORTHAND | JSON_FORMAT) => Self::Json,
            Some(format) => Self::Template(Template::parse(format, &FIELDS)?),
            None if json => Self::Json,
            None => Self::Default,
        })
    }

    fn render(&self, event: &SandboxEvent) -> Result<String> {
        match self {
            Self::Default => Ok(default_line(event)),
            Self::Json => Ok(format!("{}\n", docker_json(event))),
            Self::Template(template) => template.render(&[record(event)]),
        }
    }
}

fn event_time(event: &SandboxEvent) -> DateTime<Utc> {
    let at = event.at.unwrap_or_default();
    DateTime::from_timestamp(at.seconds, at.nanos as u32).unwrap_or_default()
}

/// `2026-10-22T10:00:00.000000000Z container start ID (image=x, name=y)`
fn default_line(event: &SandboxEvent) -> String {
    let mut attributes: Vec<String> = event
        .attributes
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    attributes.sort();
    format!(
        "{} {EVENT_TYPE} {} {} ({})\n",
        event_time(event).to_rfc3339_opts(SecondsFormat::Nanos, true),
        event.action,
        event.sandbox_id,
        attributes.join(", ")
    )
}

fn docker_json(event: &SandboxEvent) -> Value {
    let at = event_time(event);
    let from = event.attributes.get(IMAGE_ATTRIBUTE).cloned();
    json!({
        "status": event.action,
        "id": event.sandbox_id,
        "from": from,
        "Type": EVENT_TYPE,
        "Action": event.action,
        "Actor": {"ID": event.sandbox_id, "Attributes": event.attributes},
        "scope": EVENT_SCOPE,
        "time": at.timestamp(),
        "timeNano": at.timestamp_nanos_opt().unwrap_or_default(),
    })
}

/// The values a `--format` template can read.
fn record(event: &SandboxEvent) -> Map<String, Value> {
    let at = event_time(event);
    let from = event
        .attributes
        .get(IMAGE_ATTRIBUTE)
        .cloned()
        .unwrap_or_default();
    let fields = [
        ("Type", json!(EVENT_TYPE)),
        ("Action", json!(event.action)),
        ("Status", json!(event.action)),
        ("ID", json!(event.sandbox_id)),
        ("From", json!(from)),
        ("Scope", json!(EVENT_SCOPE)),
        ("Time", json!(at.timestamp())),
        (
            "TimeNano",
            json!(at.timestamp_nanos_opt().unwrap_or_default()),
        ),
        ("Actor.ID", json!(event.sandbox_id)),
    ];
    let attributes = event
        .attributes
        .iter()
        .map(|(key, value)| (format!("{ATTRIBUTES_FIELD}{key}"), json!(value)));
    fields
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .chain(attributes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789ab";

    fn event(action: &str, name: &str) -> SandboxEvent {
        SandboxEvent {
            action: action.into(),
            sandbox_id: ID.into(),
            at: Some(prost_types::Timestamp {
                seconds: 1_792_663_200,
                nanos: 5,
            }),
            attributes: [("image", "alpine"), ("name", name)]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
            dropped: 0,
        }
    }

    fn filters(items: &[&str]) -> Filters {
        Filters::parse(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn given_an_event_when_printed_by_default_then_it_reads_like_docker() {
        // Arrange
        let event = event("start", "web1");

        // Act
        let line = default_line(&event);

        // Assert
        assert_eq!(
            line,
            "2026-10-22T10:00:00.000000005Z container start 0123456789ab (image=alpine, name=web1)\n"
        );
    }

    #[test]
    fn given_a_template_when_rendered_then_fields_come_from_the_event() {
        // Arrange
        let format = Format::choose(
            Some("{{.Type}} {{.Action}} {{.Actor.ID}} {{.Actor.Attributes.name}}"),
            false,
        )
        .unwrap();

        // Act
        let line = format.render(&event("die", "web1")).unwrap();

        // Assert
        assert_eq!(line, format!("container die {ID} web1\n"));
    }

    #[test]
    fn given_json_when_rendered_then_it_is_a_docker_event_object() {
        // Arrange
        let format = Format::choose(None, true).unwrap();

        // Act
        let line = format.render(&event("die", "web1")).unwrap();

        // Assert
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["Action"], "die");
        assert_eq!(parsed["Actor"]["Attributes"]["name"], "web1");
        assert_eq!(parsed["time"], 1_792_663_200);
    }

    #[rstest::rstest]
    #[case::event_match(&["event=start"], "start", "web1", true)]
    #[case::event_miss(&["event=die"], "start", "web1", false)]
    #[case::either_event(&["event=die", "event=start"], "start", "web1", true)]
    #[case::container_by_name(&["container=web1"], "start", "web1", true)]
    #[case::container_by_id_prefix(&["container=0123"], "start", "web1", true)]
    #[case::container_miss(&["container=db"], "start", "web1", false)]
    #[case::name_alias(&["name=web1"], "start", "web1", true)]
    #[case::image_match(&["image=alpine"], "start", "web1", true)]
    #[case::image_miss(&["image=debian"], "start", "web1", false)]
    #[case::label_pair(&["label=name=web1"], "start", "web1", true)]
    #[case::label_key_only(&["label=image"], "start", "web1", true)]
    #[case::label_miss(&["label=name=db"], "start", "web1", false)]
    #[case::type_miss(&["type=image"], "start", "web1", false)]
    #[case::keys_combine(&["type=container", "event=die"], "start", "web1", false)]
    #[case::event_refused(&["event!=start"], "start", "web1", false)]
    #[case::event_not_refused(&["event!=die"], "start", "web1", true)]
    #[case::container_refused(&["container!=web1"], "start", "web1", false)]
    #[case::label_pair_refused(&["label!=name=web1"], "start", "web1", false)]
    #[case::include_and_refuse(&["event=start", "event!=start"], "start", "web1", false)]
    fn given_filters_when_checked_then_only_matching_events_pass(
        #[case] given: &[&str],
        #[case] action: &str,
        #[case] name: &str,
        #[case] allowed: bool,
    ) {
        // Arrange
        let filters = filters(given);

        // Act
        let got = filters.allows(&event(action, name));

        // Assert
        assert_eq!(got, allowed);
    }

    #[rstest::rstest]
    #[case::reason_match(&["reason=oom"], true)]
    #[case::reason_miss(&["reason=timeout"], false)]
    #[case::either_reason(&["reason=timeout", "reason=oom"], true)]
    fn given_a_reason_filter_when_checked_then_the_die_attribute_decides(
        #[case] given: &[&str],
        #[case] allowed: bool,
    ) {
        // Arrange
        let mut died = event("die", "web1");
        died.attributes.insert("reason".into(), "oom".into());

        // Act
        let got = filters(given).allows(&died);

        // Assert
        assert_eq!(got, allowed);
    }

    #[rstest::rstest]
    #[case::no_equals("event")]
    #[case::empty_value("event=")]
    #[case::unknown_key("colour=red")]
    fn given_a_bad_filter_when_parsed_then_it_is_refused(#[case] bad: &str) {
        // Arrange and Act
        let result = Filters::parse(&[bad.to_string()]);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn given_an_unknown_filter_key_when_parsed_then_the_error_reads_like_docker() {
        // Arrange and Act
        let error = Filters::parse(&["colour=red".to_string()]).err().unwrap();

        // Assert
        assert_eq!(error.to_string(), "Invalid filter 'colour'");
    }
}
