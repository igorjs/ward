// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward stats`, laid out like `docker stats`.
//!
//! CPU % comes from the guest's cumulative CPU time between two readings, so
//! 100% is one busy CPU. The guest has no per-sandbox network or block I/O
//! counters to read, so those two columns print `--`.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::{Value, json};
use tonic::Code;
use tonic::transport::Channel;
use ward_core::pb::ward_client::WardClient;
use ward_core::pb::{GetSandboxRequest, SandboxInfo, SandboxStatus, StatsRequest, StatsResponse};

use crate::client::{self, Resolved};
use crate::template::Template;

/// Time between two readings, which is what a CPU rate is measured over.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const SHORT_ID_LEN: usize = 12;
/// Shown for a value the sandbox cannot report.
const UNAVAILABLE: &str = "--";
const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const NANOS_PER_SECOND: f64 = 1e9;
const PERCENT: f64 = 100.0;
const BYTE_UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
const BYTES_PER_UNIT: f64 = 1024.0;
/// Docker prints byte sizes with this many significant digits.
const SIZE_DIGITS: usize = 4;
const JSON_FORMAT: &str = "json";
/// Each `--format` field with the header a `table` template gives it.
const FIELDS: [(&str, &str); 9] = [
    ("Container", "CONTAINER"),
    ("ID", "CONTAINER ID"),
    ("Name", "NAME"),
    ("CPUPerc", "CPU %"),
    ("MemUsage", "MEM USAGE / LIMIT"),
    ("MemPerc", "MEM %"),
    ("NetIO", "NET I/O"),
    ("BlockIO", "BLOCK I/O"),
    ("PIDs", "PIDS"),
];

/// What the caller asked `ward stats` to do.
pub struct Options {
    pub no_stream: bool,
    pub format: Option<String>,
    pub json: bool,
    pub all: bool,
    pub no_trunc: bool,
}

/// One reading of a sandbox.
type Reading = StatsResponse;

/// When the daemon took the reading, or now if it did not say.
fn taken_at(reading: &Reading) -> SystemTime {
    reading
        .read_at
        .and_then(|at| SystemTime::try_from(at).ok())
        .unwrap_or_else(SystemTime::now)
}

/// A sandbox to report on, with the token to ask for its readings.
struct Target {
    resolved: Resolved,
    name: String,
    /// What the caller typed, or the id when they named nothing.
    reference: String,
}

/// The values of one output row. `None` metrics print as `--`.
struct Row {
    id: String,
    name: String,
    reference: String,
    metrics: Option<(Reading, f64)>,
}

/// CPU use as a percentage of one CPU between two readings. A counter that
/// went backwards (the guest restarted) or no elapsed time gives 0.
fn cpu_percent(prev: &Reading, now: &Reading) -> f64 {
    let wall = taken_at(now)
        .duration_since(taken_at(prev))
        .unwrap_or_default()
        .as_secs_f64();
    if wall <= 0.0 {
        return 0.0;
    }
    let used = now.cpu_nanos.saturating_sub(prev.cpu_nanos) as f64 / NANOS_PER_SECOND;
    let ceiling = f64::from(now.online_cpus.max(1)) * PERCENT;
    (used / wall * PERCENT).min(ceiling)
}

fn mem_percent(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        return 0.0;
    }
    used as f64 / limit as f64 * PERCENT
}

/// A size in binary units with four significant digits, like Docker's.
fn format_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= BYTES_PER_UNIT && unit + 1 < BYTE_UNITS.len() {
        value /= BYTES_PER_UNIT;
        unit += 1;
    }
    let decimals = SIZE_DIGITS.saturating_sub(value.max(1.0).log10() as usize + 1);
    let text = format!("{value:.decimals$}");
    let text = if decimals > 0 {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    format!("{text}{}", BYTE_UNITS[unit])
}

/// The columns of the default table, as `--format` fields.
const DEFAULT_FORMAT: &str = "table {{.ID}}\t{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}\t{{.NetIO}}\t{{.BlockIO}}\t{{.PIDs}}";
const CELL_COUNT: usize = 8;

impl Row {
    /// The cells under `DEFAULT_FORMAT`. A sandbox that cannot be read shows `--`.
    fn cells(&self) -> Vec<String> {
        let mut cells = vec![self.id.clone(), self.name.clone()];
        match &self.metrics {
            Some((reading, cpu)) => cells.extend([
                format!("{cpu:.2}%"),
                format!(
                    "{} / {}",
                    format_bytes(reading.mem_used_bytes),
                    format_bytes(reading.mem_limit_bytes)
                ),
                format!(
                    "{:.2}%",
                    mem_percent(reading.mem_used_bytes, reading.mem_limit_bytes)
                ),
                UNAVAILABLE.to_string(),
                UNAVAILABLE.to_string(),
                reading.pids.to_string(),
            ]),
            None => cells.resize(CELL_COUNT, UNAVAILABLE.to_string()),
        }
        cells
    }

    /// The cells as Docker's JSON names them, plus the raw counters they
    /// are made from.
    fn json(&self) -> Value {
        let cells = self.cells();
        let mut object = json!({
            "Container": self.reference, "ID": cells[0], "Name": cells[1],
            "CPUPerc": cells[2], "MemUsage": cells[3], "MemPerc": cells[4],
            "NetIO": cells[5], "BlockIO": cells[6], "PIDs": cells[7],
        });
        if let Some((reading, _)) = &self.metrics {
            object["CPUNanos"] = json!(reading.cpu_nanos);
            object["OnlineCPUs"] = json!(reading.online_cpus);
            object["MemUsedBytes"] = json!(reading.mem_used_bytes);
            object["MemLimitBytes"] = json!(reading.mem_limit_bytes);
        }
        object
    }
}

fn render(rows: &[Row], template: &Template) -> String {
    let records: Vec<_> = rows
        .iter()
        .filter_map(|r| r.json().as_object().cloned())
        .collect();
    template.render(&records)
}

/// Writes one frame, first clearing the screen when `clear` is set.
fn write_frame(out: &mut impl Write, frame: &str, clear: bool) -> std::io::Result<()> {
    if clear {
        out.write_all(CLEAR_SCREEN.as_bytes())?;
    }
    out.write_all(frame.as_bytes())?;
    out.flush()
}

/// Prints readings of `references`, or of every running sandbox when none is
/// given, once with `--no-stream` and every second otherwise.
pub async fn run(
    socket: &str,
    token_store: &Path,
    references: &[String],
    options: Options,
) -> Result<()> {
    // Redrawing in place only suits the default table on a terminal.
    let mut out = std::io::stdout();
    let clear = out.is_terminal() && options.format.is_none() && !options.json;
    let format = match (options.format.as_deref(), options.json) {
        (Some(format), _) => format,
        (None, true) => JSON_FORMAT,
        (None, false) => DEFAULT_FORMAT,
    };
    let template = Template::parse(format, &FIELDS)?;
    let mut c = client::connect(socket).await?;
    let named = if references.is_empty() {
        None
    } else {
        Some(named_targets(&mut c, token_store, references).await?)
    };
    let mut session = Session {
        prev: HashMap::new(),
        strict: named.is_some(),
        all: options.all,
        no_trunc: options.no_trunc,
    };

    // The first reading only sets the baseline a rate is measured from. A
    // named sandbox that cannot be read now is an error the caller should see.
    round(&mut c, token_store, named.as_deref(), &mut session, true).await?;
    loop {
        tokio::time::sleep(SAMPLE_INTERVAL).await;
        let rows = round(&mut c, token_store, named.as_deref(), &mut session, false).await?;
        let frame = render(&rows, &template);
        write_frame(&mut out, &frame, clear && !options.no_stream)?;
        if options.no_stream {
            return Ok(());
        }
    }
}

/// One reading of every sandbox in scope: the named ones, else whichever are
/// running now.
async fn round(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    named: Option<&[Target]>,
    session: &mut Session,
    first: bool,
) -> Result<Vec<Row>> {
    let listed;
    let targets = match named {
        Some(targets) => targets,
        None => {
            listed = running_targets(c, token_store, session.all).await?;
            &listed
        }
    };
    session.read_all(c, targets, first).await
}

/// Resolves each reference, failing on the first the daemon does not know.
async fn named_targets(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    references: &[String],
) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    for reference in references {
        let info = c
            .get_sandbox(GetSandboxRequest {
                id: reference.clone(),
            })
            .await?
            .into_inner();
        targets.push(target_of(token_store, &info, reference.clone())?);
    }
    Ok(targets)
}

/// Every running sandbox (every sandbox with `all`) the local token store
/// holds a token for.
async fn running_targets(
    c: &mut WardClient<Channel>,
    token_store: &Path,
    all: bool,
) -> Result<Vec<Target>> {
    let listed = c.list_sandboxes(()).await?.into_inner().sandboxes;
    let mut targets = Vec::new();
    for info in listed
        .iter()
        .filter(|info| all || info.status == SandboxStatus::Running as i32)
    {
        let target = target_of(token_store, info, info.id.clone())?;
        if target.resolved.has_token() {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn target_of(token_store: &Path, info: &SandboxInfo, reference: String) -> Result<Target> {
    Ok(Target {
        resolved: Resolved::from_info(token_store, info)?,
        name: info.name.clone(),
        reference,
    })
}

/// Readings kept between rounds, so each round can work out a CPU rate.
struct Session {
    prev: HashMap<String, Reading>,
    /// Named sandboxes must be readable on the first round, then show `--`
    /// if they stop.
    strict: bool,
    /// Unreadable sandboxes stay as `--` rows instead of dropping out.
    all: bool,
    no_trunc: bool,
}

impl Session {
    async fn read_all(
        &mut self,
        c: &mut WardClient<Channel>,
        targets: &[Target],
        first: bool,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for target in targets {
            let id = &target.resolved.id;
            let metrics = match read(c, target).await {
                Ok(now) => {
                    let cpu = self
                        .prev
                        .get(id)
                        .map_or(0.0, |prev| cpu_percent(prev, &now));
                    self.prev.insert(id.clone(), now);
                    Some((now, cpu))
                }
                Err(e) if is_gone(&e) && !(self.strict && first) => {
                    self.prev.remove(id);
                    if !(self.strict || self.all) {
                        continue;
                    }
                    None
                }
                Err(e) => return Err(e),
            };
            rows.push(self.row(target, metrics));
        }
        Ok(rows)
    }

    fn row(&self, target: &Target, metrics: Option<(Reading, f64)>) -> Row {
        let id_len = if self.no_trunc {
            usize::MAX
        } else {
            SHORT_ID_LEN
        };
        let id = target.resolved.id.chars().take(id_len).collect();
        let name = match target.name.as_str() {
            "" => UNAVAILABLE.to_string(),
            name => name.to_string(),
        };
        Row {
            id,
            name,
            reference: target.reference.clone(),
            metrics,
        }
    }
}

async fn read(c: &mut WardClient<Channel>, target: &Target) -> Result<Reading> {
    let request = target.resolved.request(StatsRequest {
        sandbox_id: target.resolved.id.clone(),
    })?;
    Ok(c.stats(request).await?.into_inner())
}

/// The sandbox stopped or was removed since it was listed.
fn is_gone(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| matches!(status.code(), Code::NotFound | Code::InvalidArgument))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn reading(cpu_nanos: u64, online_cpus: u32, secs: i64) -> Reading {
        Reading {
            cpu_nanos,
            online_cpus,
            mem_used_bytes: 12 * MIB + MIB / 2,
            mem_limit_bytes: 1024 * MIB,
            pids: 3,
            read_at: Some(prost_types::Timestamp {
                seconds: secs,
                nanos: 0,
            }),
        }
    }

    const SECOND_NANOS: u64 = 1_000_000_000;

    #[rstest::rstest]
    #[case::half_a_cpu(SECOND_NANOS, 10, SECOND_NANOS * 3 / 2, 11, 1, 50.0)]
    #[case::no_time_passed(0, 10, SECOND_NANOS, 10, 1, 0.0)]
    fn given_two_readings_when_cpu_computed_then_percent_of_one_cpu_capped_and_never_negative(
        #[case] cpu0: u64,
        #[case] t0: i64,
        #[case] cpu1: u64,
        #[case] t1: i64,
        #[case] cpus: u32,
        #[case] expected: f64,
    ) {
        // Act
        let percent = cpu_percent(&reading(cpu0, cpus, t0), &reading(cpu1, cpus, t1));

        // Assert
        assert!((percent - expected).abs() < 1e-9, "{percent}");
    }

    #[rstest::rstest]
    #[case::zero(0, "0B")]
    #[case::fraction(1536, "1.5KiB")]
    #[case::twelve_and_a_half_mib(12 * MIB + MIB / 2, "12.5MiB")]
    #[case::four_digits_of_precision(1_234_567_890, "1.15GiB")]
    fn given_a_byte_count_when_formatted_then_binary_units_with_four_digits(
        #[case] bytes: u64,
        #[case] expected: &str,
    ) {
        assert_eq!(format_bytes(bytes), expected);
    }

    #[rstest::rstest]
    #[case::terminal(true, "\x1b[2J\x1b[Hframe")]
    #[case::piped(false, "frame")]
    fn given_a_frame_when_written_then_the_screen_is_cleared_only_when_asked(
        #[case] clear: bool,
        #[case] expected: &str,
    ) {
        let mut out = Vec::new();

        write_frame(&mut out, "frame", clear).expect("write");

        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }
}
