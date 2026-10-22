// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward ps`, `ward inspect`, `ward kill` and the signal names `-s` accepts.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{GetSandboxRequest, KillProcessRequest, SandboxInfo, SandboxStatus};

use crate::lifecycle::Failures;
use crate::{client, output, status_name};

pub const PS_HEADERS: [&str; 6] = [
    "CONTAINER ID",
    "IMAGE",
    "COMMAND",
    "CREATED",
    "STATUS",
    "NAMES",
];

/// Docker shows the first 12 characters of an id.
const SHORT_ID_LEN: usize = 12;
const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_DAY: i64 = 86_400;
// Constants of the days to civil date algorithm (Howard Hinnant, "chrono-Compatible
// Low-Level Date Algorithms"). Years start in March, so February ends the year.
const DAYS_FROM_YEAR_0_TO_1970: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;
const DAYS_PER_YEAR: i64 = 365;
const DAYS_PER_4_YEARS: i64 = 1460;
const DAYS_PER_100_YEARS: i64 = 36_524;
const DAYS_PER_ERA_LESS_ONE: i64 = 146_096;
const DAYS_PER_5_MONTHS: i64 = 153;
/// Index of the first month after the March based year's ten month point (January).
const JANUARY_INDEX: i64 = 10;
const MARCH_NUMBER: i64 = 3;
const JANUARY_NUMBER_OFFSET: i64 = 9;

/// Signal numbers on Linux, which is what the guest runs.
const SIGNALS: [(&str, i32); 30] = [
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("XCPU", 24),
    ("XFSZ", 25),
    ("VTALRM", 26),
    ("PROF", 27),
    ("WINCH", 28),
    ("IO", 29),
    ("PWR", 30),
    ("SYS", 31),
];
/// Highest signal number Linux accepts (the last real-time signal).
const MAX_SIGNAL: i32 = 64;

/// Accept `TERM`, `SIGTERM` or a number, like `docker kill -s`.
pub fn parse_signal(text: &str) -> Result<i32> {
    if let Ok(number) = text.parse::<i32>() {
        if (1..=MAX_SIGNAL).contains(&number) {
            return Ok(number);
        }
        bail!("invalid signal: {text}");
    }
    let name = text.to_ascii_uppercase();
    let name = name.strip_prefix("SIG").unwrap_or(&name);
    match SIGNALS.iter().find(|(known, _)| *known == name) {
        Some((_, number)) => Ok(*number),
        None => bail!("invalid signal: {text}"),
    }
}

/// Docker's wording for an elapsed time, such as `5 minutes` or `About an hour`.
fn duration_text(seconds: i64) -> String {
    let plural = |count: i64, unit: &str| {
        let s = if count == 1 { "" } else { "s" };
        format!("{count} {unit}{s}")
    };
    match seconds {
        s if s < 1 => "Less than a second".to_string(),
        s if s < SECONDS_PER_MINUTE => plural(s, "second"),
        s if s < 2 * SECONDS_PER_MINUTE => "About a minute".to_string(),
        s if s < SECONDS_PER_HOUR => plural(s / SECONDS_PER_MINUTE, "minute"),
        s if s < 2 * SECONDS_PER_HOUR => "About an hour".to_string(),
        s if s < SECONDS_PER_DAY => plural(s / SECONDS_PER_HOUR, "hour"),
        s => plural(s / SECONDS_PER_DAY, "day"),
    }
}

/// Seconds from `time` to `now_seconds`, never negative.
fn seconds_since(time: &Option<prost_types::Timestamp>, now_seconds: i64) -> Option<i64> {
    time.as_ref().map(|t| (now_seconds - t.seconds).max(0))
}

/// Docker style status: `Up 5 minutes`, `Exited (7) 5 minutes ago`, `Created`.
/// `Up` counts from the last start, falling back to the creation time for
/// records that predate it. `Exited` counts from the finish and shows only the
/// code when there is none.
fn status_text(info: &SandboxInfo, now_seconds: i64) -> String {
    let since_created = seconds_since(&info.created_at, now_seconds).unwrap_or_default();
    match SandboxStatus::try_from(info.status).unwrap_or(SandboxStatus::Unspecified) {
        SandboxStatus::Running => {
            let uptime = seconds_since(&info.started_at, now_seconds).unwrap_or(since_created);
            format!("Up {}", duration_text(uptime))
        }
        SandboxStatus::Exited => {
            let code = info.exit_code.unwrap_or_default();
            match seconds_since(&info.finished_at, now_seconds) {
                Some(ago) => format!("Exited ({code}) {} ago", duration_text(ago)),
                None => format!("Exited ({code})"),
            }
        }
        SandboxStatus::Creating => "Created".to_string(),
        SandboxStatus::Stopped => "Stopped".to_string(),
        SandboxStatus::Failed => "Failed".to_string(),
        SandboxStatus::Stopping => "Stopping".to_string(),
        SandboxStatus::Unspecified => "Unknown".to_string(),
    }
}

/// One `ps` row. Only argv[0] of the command is shown, since arguments can
/// hold secrets.
pub fn ps_row(info: &SandboxInfo, now_seconds: i64) -> Vec<Value> {
    let elapsed = seconds_since(&info.created_at, now_seconds).unwrap_or_default();
    let created = format!("{} ago", duration_text(elapsed));
    let short_id: String = info.id.chars().take(SHORT_ID_LEN).collect();
    vec![
        Value::String(short_id),
        Value::String(info.image.clone()),
        Value::String(info.command_name.clone()),
        Value::String(created),
        Value::String(status_text(info, now_seconds)),
        Value::String(info.name.clone()),
    ]
}

/// One `ps --format json` object, keyed as Docker names the fields.
pub fn ps_json_object(info: &SandboxInfo, now_seconds: i64) -> Value {
    let created = info.created_at.as_ref().map(|t| t.seconds);
    let elapsed = seconds_since(&info.created_at, now_seconds).unwrap_or_default();
    let short_id: String = info.id.chars().take(SHORT_ID_LEN).collect();
    json!({
        "ID": short_id,
        "Image": info.image,
        "Command": info.command_name,
        "CreatedAt": created.map(created_at_text),
        "RunningFor": format!("{} ago", duration_text(elapsed)),
        "Status": status_text(info, now_seconds),
        "Names": info.name,
    })
}

pub struct PsOptions {
    pub all: bool,
    pub quiet: bool,
    /// Global `--json`: one array.
    pub json: bool,
    /// `--format json`: one object per line, like Docker.
    pub format_json: bool,
    pub no_pretty: bool,
}

pub async fn ps(c: &mut WardClient<Channel>, options: PsOptions) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let shown: Vec<&SandboxInfo> = listed
        .iter()
        .filter(|s| options.all || s.status == SandboxStatus::Running as i32)
        .collect();
    if options.quiet {
        for info in shown {
            println!("{}", info.id);
        }
        return Ok(());
    }
    if options.format_json {
        for info in shown {
            println!("{}", ps_json_object(info, now));
        }
        return Ok(());
    }
    if options.json {
        let objects: Vec<Value> = shown.into_iter().map(|s| ps_json_object(s, now)).collect();
        println!("{}", serde_json::to_string(&objects)?);
        return Ok(());
    }
    let rows: Vec<Vec<Value>> = shown.into_iter().map(|s| ps_row(s, now)).collect();
    if let Some(header) = output::tsv_header(options.no_pretty, &PS_HEADERS) {
        println!("{header}");
    }
    output::emit_rows(false, options.no_pretty, &PS_HEADERS, &rows)
}

/// Docker shaped `inspect` entry for one sandbox.
fn inspect_value(info: &SandboxInfo) -> Value {
    let entrypoint_len = info.entrypoint.len();
    let cmd: Vec<&str> = if info.command.is_empty() {
        // A sandbox with no main process has an empty command name.
        Some(info.command_name.as_str())
            .filter(|name| !name.is_empty())
            .into_iter()
            .collect()
    } else {
        info.command
            .iter()
            .skip(entrypoint_len)
            .map(String::as_str)
            .collect()
    };
    // Docker reports an unset entrypoint as null.
    let entrypoint = (entrypoint_len > 0).then_some(&info.entrypoint);
    json!({
        "Id": info.id,
        "Name": format!("/{}", info.name),
        "Created": info.created_at.as_ref().map(|t| rfc3339(t.seconds)),
        "State": {
            "Status": status_name(info.status),
            "ExitCode": info.exit_code.unwrap_or_default(),
            "StartedAt": state_time(&info.started_at),
            "FinishedAt": state_time(&info.finished_at),
        },
        "Config": {
            "Image": info.image,
            "Cmd": cmd,
            "Entrypoint": entrypoint,
            "WorkingDir": info.workdir,
            "User": info.user,
        },
    })
}

/// Calendar fields of a UTC instant.
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

/// Split unix `seconds` into UTC calendar fields, avoiding a date crate.
fn civil(seconds: i64) -> Civil {
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let rest = seconds.rem_euclid(SECONDS_PER_DAY);
    let shifted = days + DAYS_FROM_YEAR_0_TO_1970;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = shifted.rem_euclid(DAYS_PER_ERA);
    let year_of_era = (day_of_era - day_of_era / DAYS_PER_4_YEARS
        + day_of_era / DAYS_PER_100_YEARS
        - day_of_era / DAYS_PER_ERA_LESS_ONE)
        / DAYS_PER_YEAR;
    let day_of_year =
        day_of_era - (DAYS_PER_YEAR * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / DAYS_PER_5_MONTHS;
    let day = day_of_year - (DAYS_PER_5_MONTHS * month_index + 2) / 5 + 1;
    let month = if month_index < JANUARY_INDEX {
        month_index + MARCH_NUMBER
    } else {
        month_index - JANUARY_NUMBER_OFFSET
    };
    Civil {
        year: year_of_era + era * YEARS_PER_ERA + i64::from(month <= 2),
        month,
        day,
        hour: rest / SECONDS_PER_HOUR,
        minute: rest % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
        second: rest % SECONDS_PER_MINUTE,
    }
}

fn rfc3339(seconds: i64) -> String {
    let c = civil(seconds);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

/// What Docker prints for a state time that was never set.
const UNSET_TIME: &str = "0001-01-01T00:00:00Z";
const NANOS_DIGITS: usize = 9;

/// Docker's RFC3339Nano: the fraction drops its trailing zeros, and an unset
/// time is the zero time.
fn state_time(time: &Option<prost_types::Timestamp>) -> String {
    let Some(t) = time else {
        return UNSET_TIME.to_string();
    };
    let whole = rfc3339(t.seconds);
    let fraction = format!("{:0width$}", t.nanos, width = NANOS_DIGITS);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        return whole;
    }
    let date = whole.trim_end_matches('Z');
    format!("{date}.{fraction}Z")
}

/// Docker's `CreatedAt` layout, for example `2024-02-29 00:00:00 +0000 UTC`.
fn created_at_text(seconds: i64) -> String {
    let c = civil(seconds);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} +0000 UTC",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

/// Print one array element per found reference. Missing ones go to stderr
/// and make the command fail, like Docker.
pub async fn inspect(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    references: &[String],
) -> Result<()> {
    let mut found = Vec::new();
    let mut failures = Failures::new();
    for reference in references {
        let result = async {
            let target = client::resolve(c, token_store, reference).await?;
            let request = target.request(GetSandboxRequest {
                id: target.id.clone(),
            })?;
            anyhow::Ok(c.get_sandbox(request).await?.into_inner())
        }
        .await;
        match result {
            Ok(info) => found.push(inspect_value(&info)),
            Err(e) => failures.record(reference, &e),
        }
    }
    println!("{}", serde_json::to_string_pretty(&found)?);
    failures.finish(references.len(), "inspected")
}

/// Send `signal` to `pid` in each sandbox, printing every reference signalled.
pub async fn kill(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    references: &[String],
    pid: &str,
    signal: i32,
    json: bool,
) -> Result<()> {
    let mut failures = Failures::new();
    for reference in references {
        let result = async {
            let target = client::resolve(c, token_store, reference).await?;
            let request = target.request(KillProcessRequest {
                sandbox_id: target.id.clone(),
                pid: pid.to_string(),
                signal,
            })?;
            c.kill_process(request).await?;
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) if json => println!("{}", json!({"killed": reference})),
            Ok(()) => println!("{reference}"),
            Err(e) => failures.record(reference, &e),
        }
    }
    failures.finish(references.len(), "killed")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn info(status: SandboxStatus, exit_code: Option<i32>) -> SandboxInfo {
        SandboxInfo {
            id: "0123456789abcdef0123".into(),
            name: "web1".into(),
            image: "alpine".into(),
            command_name: "sh".into(),
            status: status as i32,
            exit_code,
            created_at: Some(prost_types::Timestamp {
                seconds: NOW - 120,
                nanos: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn given_ps_when_headers_read_then_order_is_docker_order() {
        assert_eq!(
            PS_HEADERS,
            [
                "CONTAINER ID",
                "IMAGE",
                "COMMAND",
                "CREATED",
                "STATUS",
                "NAMES"
            ]
        );
    }

    #[test]
    fn given_exited_sandbox_when_row_built_then_row_follows_header_order() {
        // Arrange
        let sandbox = info(SandboxStatus::Exited, Some(7));

        // Act
        let row = ps_row(&sandbox, NOW);

        // Assert
        let cells: Vec<&str> = row.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            cells,
            [
                "0123456789ab",
                "alpine",
                "sh",
                "2 minutes ago",
                "Exited (7)",
                "web1"
            ]
        );
    }

    #[test]
    fn given_sandbox_when_json_object_built_then_created_at_is_absolute_and_running_for_relative() {
        // Arrange
        let sandbox = info(SandboxStatus::Running, None);

        // Act
        let object = ps_json_object(&sandbox, NOW);

        // Assert
        assert_eq!(
            object,
            json!({
                "ID": "0123456789ab",
                "Image": "alpine",
                "Command": "sh",
                "CreatedAt": "1970-01-12 13:44:40 +0000 UTC",
                "RunningFor": "2 minutes ago",
                "Status": "Up 2 minutes",
                "Names": "web1",
            })
        );
    }

    #[test]
    fn given_command_with_arguments_when_row_built_then_only_argv0_is_shown() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);
        sandbox.command = vec!["sh".into(), "--password=hunter2".into()];

        // Act
        let row = ps_row(&sandbox, NOW);

        // Assert
        assert_eq!(row[2], "sh");
    }

    #[test]
    fn given_command_when_inspected_then_cmd_is_full_argv_else_argv0() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);

        // Act and Assert
        assert_eq!(inspect_value(&sandbox)["Config"]["Cmd"], json!(["sh"]));
        sandbox.command = vec!["sh".into(), "-c".into(), "read x".into()];
        assert_eq!(
            inspect_value(&sandbox)["Config"]["Cmd"],
            json!(["sh", "-c", "read x"])
        );
    }

    #[test]
    fn given_exited_sandbox_when_inspected_then_value_follows_docker_shape() {
        // Arrange
        let sandbox = info(SandboxStatus::Exited, Some(7));

        // Act
        let value = inspect_value(&sandbox);

        // Assert
        assert_eq!(
            value,
            json!({
                "Id": "0123456789abcdef0123",
                "Name": "/web1",
                "Created": "1970-01-12T13:44:40Z",
                "State": {
                    "Status": "exited",
                    "ExitCode": 7,
                    "StartedAt": "0001-01-01T00:00:00Z",
                    "FinishedAt": "0001-01-01T00:00:00Z",
                },
                "Config": {
                    "Image": "alpine",
                    "Cmd": ["sh"],
                    "Entrypoint": null,
                    "WorkingDir": "",
                    "User": "",
                },
            })
        );
    }

    #[test]
    fn given_entrypoint_and_workdir_when_inspected_then_config_splits_entrypoint_from_cmd() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);
        sandbox.entrypoint = vec!["/entry".into()];
        sandbox.command = vec!["/entry".into(), "serve".into()];
        sandbox.workdir = "/app".into();

        // Act
        let config = inspect_value(&sandbox)["Config"].clone();

        // Assert
        assert_eq!(config["Entrypoint"], json!(["/entry"]));
        assert_eq!(config["Cmd"], json!(["serve"]));
        assert_eq!(config["WorkingDir"], "/app");
    }

    #[test]
    fn given_sandbox_without_main_process_when_inspected_then_cmd_is_empty() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);
        sandbox.command_name = String::new();

        // Act
        let config = inspect_value(&sandbox)["Config"].clone();

        // Assert
        assert_eq!(config["Cmd"], json!([]));
    }

    #[test]
    fn given_image_user_when_inspected_then_config_shows_it() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);
        sandbox.user = "app".into();

        // Act
        let config = inspect_value(&sandbox)["Config"].clone();

        // Assert
        assert_eq!(config["User"], "app");
    }

    #[rstest::rstest]
    #[case::epoch(0, "1970-01-01T00:00:00Z")]
    #[case::leap_day(1_709_164_800, "2024-02-29T00:00:00Z")]
    #[case::year_end(1_767_225_599, "2025-12-31T23:59:59Z")]
    fn given_seconds_when_formatted_then_rfc3339_utc(#[case] seconds: i64, #[case] want: &str) {
        assert_eq!(rfc3339(seconds), want);
    }

    #[rstest::rstest]
    #[case::none(0, "Less than a second")]
    #[case::one_second(1, "1 second")]
    #[case::seconds(59, "59 seconds")]
    #[case::about_a_minute(60, "About a minute")]
    #[case::minutes(300, "5 minutes")]
    #[case::about_an_hour(3600, "About an hour")]
    #[case::hours(7200, "2 hours")]
    #[case::days(172_800, "2 days")]
    fn given_elapsed_seconds_when_formatted_then_docker_wording(
        #[case] seconds: i64,
        #[case] want: &str,
    ) {
        assert_eq!(duration_text(seconds), want);
    }

    #[rstest::rstest]
    #[case::running(SandboxStatus::Running, None, "Up 2 minutes")]
    #[case::exited(SandboxStatus::Exited, Some(143), "Exited (143)")]
    #[case::creating(SandboxStatus::Creating, None, "Created")]
    #[case::stopped(SandboxStatus::Stopped, None, "Stopped")]
    fn given_state_when_status_built_then_docker_wording(
        #[case] status: SandboxStatus,
        #[case] exit_code: Option<i32>,
        #[case] want: &str,
    ) {
        assert_eq!(status_text(&info(status, exit_code), NOW), want);
    }

    #[test]
    fn given_restarted_sandbox_when_status_built_then_up_counts_from_started_at() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Running, None);
        sandbox.started_at = Some(prost_types::Timestamp {
            seconds: NOW - 60,
            nanos: 0,
        });

        // Act
        let status = status_text(&sandbox, NOW);

        // Assert
        assert_eq!(status, "Up About a minute");
    }

    #[test]
    fn given_finished_sandbox_when_status_built_then_exited_counts_from_finished_at() {
        // Arrange
        let mut sandbox = info(SandboxStatus::Exited, Some(7));
        sandbox.finished_at = Some(prost_types::Timestamp {
            seconds: NOW - 300,
            nanos: 0,
        });

        // Act
        let status = status_text(&sandbox, NOW);

        // Assert
        assert_eq!(status, "Exited (7) 5 minutes ago");
    }

    #[rstest::rstest]
    #[case::unset(None, "0001-01-01T00:00:00Z")]
    #[case::whole_second(Some((0, 0)), "1970-01-01T00:00:00Z")]
    #[case::trimmed_fraction(Some((0, 120_000_000)), "1970-01-01T00:00:00.12Z")]
    #[case::full_fraction(Some((0, 1)), "1970-01-01T00:00:00.000000001Z")]
    fn given_state_time_when_formatted_then_rfc3339_nano(
        #[case] time: Option<(i64, i32)>,
        #[case] want: &str,
    ) {
        let time = time.map(|(seconds, nanos)| prost_types::Timestamp { seconds, nanos });
        assert_eq!(state_time(&time), want);
    }

    #[rstest::rstest]
    #[case::bare("TERM", Some(15))]
    #[case::prefixed("SIGKILL", Some(9))]
    #[case::lower("int", Some(2))]
    #[case::number("10", Some(10))]
    #[case::zero("0", None)]
    #[case::table_end("SIGSYS", Some(31))]
    #[case::realtime_max("64", Some(64))]
    #[case::past_max("65", None)]
    #[case::mixed_case("SigChld", Some(17))]
    #[case::unknown("NOPE", None)]
    fn given_signal_text_when_parsed_then_number_or_error(
        #[case] text: &str,
        #[case] want: Option<i32>,
    ) {
        assert_eq!(parse_signal(text).ok(), want);
    }
}
