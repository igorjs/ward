// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! What `ward logs` prints: the `--tail`, `--since` and `--timestamps`
//! options applied to the Attach output.

use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use ward_core::pb::AttachStream;

use crate::attach::{Chunk, Output};

const STREAM_COUNT: usize = 2;
const SECONDS_PER_NANOSECOND: f64 = 1e-9;
const SECONDS_PER_MICROSECOND: f64 = 1e-6;
const SECONDS_PER_MILLISECOND: f64 = 1e-3;
const SECONDS_PER_MINUTE: f64 = 60.0;
const SECONDS_PER_HOUR: f64 = 3600.0;
const NANOS_DIGITS: usize = 9;
const DATE_FORMAT: &str = "%Y-%m-%d";
const DATE_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";
const OLD_DAEMON_WARNING: &str = "warning: the daemon does not report log times; upgrade wardd\n";
/// Go duration units, as `docker logs --since` accepts them.
const DURATION_UNITS: [(&str, f64); 7] = [
    ("ns", SECONDS_PER_NANOSECOND),
    ("us", SECONDS_PER_MICROSECOND),
    ("\u{b5}s", SECONDS_PER_MICROSECOND),
    ("ms", SECONDS_PER_MILLISECOND),
    ("s", 1.0),
    ("m", SECONDS_PER_MINUTE),
    ("h", SECONDS_PER_HOUR),
];

/// The log options that shape the printed output.
pub struct LogOptions {
    pub tail: Option<usize>,
    pub since: Option<DateTime<Utc>>,
    pub timestamps: bool,
}

/// A replayed chunk held back until the replay ends and `--tail` can trim it.
pub struct Held {
    stream: AttachStream,
    at: Option<DateTime<Utc>>,
    bytes: Vec<u8>,
}

/// Bytes to write and the stream to write them to.
pub type Printable = (AttachStream, Vec<u8>);

pub struct LogPrinter {
    options: LogOptions,
    replaying: bool,
    held: Vec<Held>,
    at_line_start: [bool; STREAM_COUNT],
    warned_old_daemon: bool,
}

impl LogPrinter {
    pub fn new(options: LogOptions) -> Self {
        Self {
            replaying: options.tail.is_some(),
            options,
            held: Vec::new(),
            at_line_start: [true; STREAM_COUNT],
            warned_old_daemon: false,
        }
    }

    /// What to print now for the next piece of the stream.
    pub fn accept(&mut self, output: Output) -> Vec<Printable> {
        match output {
            Output::Data(chunk) => self.accept_data(chunk),
            Output::ReplayEnd => self.finish(),
        }
    }

    /// Releases whatever `--tail` still holds; the replay is over.
    pub fn finish(&mut self) -> Vec<Printable> {
        self.replaying = false;
        let held = std::mem::take(&mut self.held);
        let Some(lines) = self.options.tail else {
            return Vec::new();
        };
        tail_chunks(&held, lines)
            .into_iter()
            .map(|(chunk, bytes)| self.render(chunk.stream, chunk.at, bytes))
            .collect()
    }

    fn accept_data(&mut self, chunk: Chunk) -> Vec<Printable> {
        let mut printed = if chunk.at.is_none() {
            self.daemon_is_old()
        } else {
            Vec::new()
        };
        printed.extend(self.accept_timed(chunk));
        printed
    }

    /// A chunk without a time means the daemon predates log times and will
    /// never send a replay marker: warn once if times were asked for, and
    /// release what `--tail` holds.
    fn daemon_is_old(&mut self) -> Vec<Printable> {
        let mut printed = Vec::new();
        let wants_times = self.options.since.is_some() || self.options.timestamps;
        if wants_times && !self.warned_old_daemon {
            self.warned_old_daemon = true;
            printed.push((AttachStream::Stderr, OLD_DAEMON_WARNING.as_bytes().to_vec()));
        }
        if self.replaying {
            printed.extend(self.finish());
        }
        printed
    }

    fn accept_timed(&mut self, chunk: Chunk) -> Vec<Printable> {
        if let (Some(since), Some(at)) = (self.options.since, chunk.at)
            && at < since
        {
            return Vec::new();
        }
        if self.replaying {
            self.held.push(Held {
                stream: chunk.stream,
                at: chunk.at,
                bytes: chunk.bytes.to_vec(),
            });
            return Vec::new();
        }
        vec![self.render(chunk.stream, chunk.at, chunk.bytes)]
    }

    fn render(
        &mut self,
        stream: AttachStream,
        at: Option<DateTime<Utc>>,
        bytes: &[u8],
    ) -> Printable {
        let Some(at) = at.filter(|_| self.options.timestamps) else {
            return (stream, bytes.to_vec());
        };
        let stamp = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let line_start = &mut self.at_line_start[stream as usize];
        (stream, prefix_lines(&stamp, bytes, line_start))
    }
}

/// `bytes` with `stamp` and a space before every line. `at_line_start` says
/// whether the bytes begin a line and is updated for the next call.
fn prefix_lines(stamp: &str, bytes: &[u8], at_line_start: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + stamp.len() + 1);
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        if *at_line_start {
            out.extend_from_slice(stamp.as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(line);
        *at_line_start = line.ends_with(b"\n");
    }
    out
}

/// The parts of `chunks` that make up their last `lines` lines, each paired
/// with the chunk it came from.
fn tail_chunks(chunks: &[Held], lines: usize) -> Vec<(&Held, &[u8])> {
    let all: Vec<u8> = chunks
        .iter()
        .flat_map(|chunk| &chunk.bytes)
        .copied()
        .collect();
    let mut skip = all.len() - last_lines(&all, lines).len();
    let mut kept = Vec::new();
    for chunk in chunks {
        let dropped = skip.min(chunk.bytes.len());
        skip -= dropped;
        if dropped < chunk.bytes.len() {
            kept.push((chunk, &chunk.bytes[dropped..]));
        }
    }
    kept
}

/// The last `count` lines of `output`, keeping their line endings.
fn last_lines(output: &[u8], count: usize) -> &[u8] {
    let body = output.strip_suffix(b"\n").unwrap_or(output);
    let mut start = body.len();
    for _ in 0..count {
        match body[..start].iter().rposition(|&b| b == b'\n') {
            Some(newline) => start = newline,
            None => return output,
        }
    }
    if count == 0 {
        return &[];
    }
    &output[start + 1..]
}

/// Parses `--since` the way `docker logs` does: a Go style duration (`10m`,
/// `1h30m`) counted back from `now`, an RFC3339 timestamp, a date or date
/// time without a zone (local time), or Unix seconds with an optional fraction.
pub fn parse_since(value: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    parse_duration(value)
        .and_then(|ago| chrono::Duration::from_std(ago).ok())
        .and_then(|ago| now.checked_sub_signed(ago))
        .or_else(|| parse_timestamp(value))
        .ok_or_else(|| {
            anyhow!(
                "invalid value '{value}' for --since: use a duration like 10m, a timestamp like 2026-01-02T03:04:05Z, or Unix seconds"
            )
        })
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Some(at.with_timezone(&Utc));
    }
    let local = NaiveDateTime::parse_from_str(value, DATE_TIME_FORMAT)
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(value, DATE_FORMAT)
                .ok()?
                .and_hms_opt(0, 0, 0)
        });
    match local {
        Some(local) => Some(
            local
                .and_local_timezone(Local)
                .earliest()?
                .with_timezone(&Utc),
        ),
        None => parse_unix_seconds(value),
    }
}

/// Unix seconds, optionally with a decimal fraction (`1600000000.5`).
fn parse_unix_seconds(value: &str) -> Option<DateTime<Utc>> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let all_digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let digits: String = fraction.chars().take(NANOS_DIGITS).collect();
    let nanos = format!("{digits:0<NANOS_DIGITS$}").parse().ok()?;
    DateTime::from_timestamp(whole.parse().ok()?, nanos)
}

fn parse_duration(value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    let is_number = |c: char| c.is_ascii_digit() || c == '.';
    let mut rest = value;
    let mut seconds = 0.0;
    while !rest.is_empty() {
        let (number, after) = rest.split_at(rest.find(|c| !is_number(c))?);
        let (unit, next) = after.split_at(after.find(is_number).unwrap_or(after.len()));
        let (_, per_unit) = DURATION_UNITS.iter().find(|(name, _)| *name == unit)?;
        seconds += number.parse::<f64>().ok()? * per_unit;
        rest = next;
    }
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const STAMP: &str = "2026-10-22T10:00:00.123456789Z";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn data<'a>(stream: AttachStream, bytes: &'a [u8], time: &str) -> Output<'a> {
        Output::Data(Chunk {
            stream,
            bytes,
            at: Some(at(time)),
        })
    }

    fn text(printed: &[Printable]) -> String {
        let bytes: Vec<u8> = printed.iter().flat_map(|(_, b)| b.clone()).collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn given_chunks_split_mid_line_when_prefixed_then_each_line_is_stamped_once() {
        // Arrange
        let mut at_line_start = true;

        // Act
        let first = prefix_lines(STAMP, b"a\nb", &mut at_line_start);
        let second = prefix_lines(STAMP, b"c\nd\n", &mut at_line_start);

        // Assert
        assert_eq!(first, format!("{STAMP} a\n{STAMP} b").into_bytes());
        assert_eq!(second, format!("c\n{STAMP} d\n").into_bytes());
        assert!(at_line_start);
    }

    #[test]
    fn given_timestamps_on_when_a_chunk_is_printed_then_lines_carry_the_arrival_time() {
        // Arrange
        let mut printer = LogPrinter::new(LogOptions {
            tail: None,
            since: None,
            timestamps: true,
        });

        // Act
        let printed = printer.accept(data(AttachStream::Stderr, b"x\ny\n", STAMP));

        // Assert
        assert_eq!(printed[0].0, AttachStream::Stderr);
        assert_eq!(text(&printed), format!("{STAMP} x\n{STAMP} y\n"));
    }

    #[test]
    fn given_follow_with_tail_when_replay_ends_then_only_the_last_lines_print_before_live_output() {
        // Arrange
        let mut printer = LogPrinter::new(LogOptions {
            tail: Some(2),
            since: None,
            timestamps: false,
        });
        let held = printer.accept(data(AttachStream::Stdout, b"1\n2\n3\n", STAMP));

        // Act
        let replayed = printer.accept(Output::ReplayEnd);
        let live = printer.accept(data(AttachStream::Stdout, b"4\n", STAMP));

        // Assert
        assert!(held.is_empty());
        assert_eq!(text(&replayed), "2\n3\n");
        assert_eq!(text(&live), "4\n");
    }

    #[test]
    fn given_since_when_older_chunks_arrive_then_they_are_dropped() {
        // Arrange
        let mut printer = LogPrinter::new(LogOptions {
            tail: None,
            since: Some(at("2026-10-22T10:00:00Z")),
            timestamps: false,
        });

        // Act
        let old = printer.accept(data(AttachStream::Stdout, b"old\n", "2026-10-22T09:59:59Z"));
        let edge = printer.accept(data(AttachStream::Stdout, b"new\n", "2026-10-22T10:00:00Z"));

        // Assert
        assert!(old.is_empty());
        assert_eq!(text(&edge), "new\n");
    }

    #[rstest]
    #[case("45s", 45.0)]
    #[case("10m", 600.0)]
    #[case("1h30m", 5400.0)]
    #[case("1.5h", 5400.0)]
    #[case("250ms", 0.25)]
    fn given_a_go_duration_when_parsed_then_it_counts_back_from_now(
        #[case] value: &str,
        #[case] seconds: f64,
    ) {
        // Arrange
        let now = at("2026-10-22T12:00:00Z");

        // Act
        let since = parse_since(value, now).unwrap();

        // Assert
        let ago = (now - since).num_milliseconds() as f64 / 1000.0;
        assert!((ago - seconds).abs() < 1e-6, "{value} gave {ago}");
    }

    #[test]
    fn given_an_rfc3339_timestamp_when_parsed_then_it_is_used_as_is() {
        // Arrange
        let now = at("2026-10-22T12:00:00Z");

        // Act
        let since = parse_since("2026-10-22T10:00:00+02:00", now).unwrap();

        // Assert
        assert_eq!(since, at("2026-10-22T08:00:00Z"));
    }

    #[rstest]
    #[case("1600000000", "2020-09-13T12:26:40Z")]
    #[case("1600000000.5", "2020-09-13T12:26:40.5Z")]
    #[case("10", "1970-01-01T00:00:10Z")]
    #[case("2026-01-02T03:04:05.25Z", "2026-01-02T03:04:05.25Z")]
    fn given_unix_seconds_or_a_zoned_timestamp_when_parsed_then_it_is_that_instant(
        #[case] value: &str,
        #[case] expected: &str,
    ) {
        // Arrange
        let now = at("2026-10-22T12:00:00Z");

        // Act
        let since = parse_since(value, now).unwrap();

        // Assert
        assert_eq!(since, at(expected));
    }

    #[rstest]
    #[case("2026-01-02", "2026-01-02T00:00:00")]
    #[case("2026-01-02T03:04:05", "2026-01-02T03:04:05")]
    #[case("2026-01-02T03:04:05.5", "2026-01-02T03:04:05.5")]
    fn given_a_timestamp_without_a_zone_when_parsed_then_it_is_local_time(
        #[case] value: &str,
        #[case] wall_clock: &str,
    ) {
        // Arrange
        let now = at("2026-10-22T12:00:00Z");
        let expected = NaiveDateTime::parse_from_str(wall_clock, DATE_TIME_FORMAT).unwrap();

        // Act
        let since = parse_since(value, now).unwrap();

        // Assert
        assert_eq!(since.with_timezone(&Local).naive_local(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("m")]
    #[case("5x")]
    #[case("1h-5m")]
    #[case("yesterday")]
    fn given_an_invalid_since_when_parsed_then_it_fails_with_the_value(#[case] value: &str) {
        // Arrange
        let now = at("2026-10-22T12:00:00Z");

        // Act
        let error = parse_since(value, now).unwrap_err().to_string();

        // Assert
        assert!(error.contains(&format!("'{value}'")), "{error}");
    }

    #[test]
    fn given_follow_with_tail_when_a_chunk_has_no_time_then_held_output_is_released_without_a_marker()
     {
        // Arrange
        let mut printer = LogPrinter::new(LogOptions {
            tail: Some(1),
            since: None,
            timestamps: false,
        });
        let untimed = |bytes| {
            Output::Data(Chunk {
                stream: AttachStream::Stdout,
                bytes,
                at: None,
            })
        };
        let held = printer.accept(data(AttachStream::Stdout, b"1\n2\n", STAMP));

        // Act
        let after = printer.accept(untimed(b"3\n"));

        // Assert
        assert!(held.is_empty());
        assert_eq!(text(&after), "2\n3\n");
    }

    #[rstest]
    #[case(Some("2026-10-22T10:00:00Z"), false, 1)]
    #[case(None, true, 1)]
    #[case(None, false, 0)]
    fn given_chunks_without_a_time_when_times_are_wanted_then_one_warning_goes_to_stderr(
        #[case] since: Option<&str>,
        #[case] timestamps: bool,
        #[case] warnings: usize,
    ) {
        // Arrange
        let mut printer = LogPrinter::new(LogOptions {
            tail: None,
            since: since.map(at),
            timestamps,
        });
        let untimed = || {
            Output::Data(Chunk {
                stream: AttachStream::Stdout,
                bytes: b"x\n",
                at: None,
            })
        };

        // Act
        let first = printer.accept(untimed());
        let second = printer.accept(untimed());

        // Assert
        let stderr: Vec<_> = first
            .iter()
            .chain(&second)
            .filter(|(stream, _)| *stream == AttachStream::Stderr)
            .collect();
        assert_eq!(stderr.len(), warnings);
        if warnings == 1 {
            assert_eq!(stderr[0].1, OLD_DAEMON_WARNING.as_bytes());
        }
        assert!(text(&first).ends_with("x\n"));
    }

    #[test]
    fn given_mixed_streams_when_tailed_then_each_kept_part_keeps_its_stream() {
        // Arrange
        let held = |stream, bytes: &[u8]| Held {
            stream,
            at: None,
            bytes: bytes.to_vec(),
        };
        let chunks = vec![
            held(AttachStream::Stdout, b"a\nb"),
            held(AttachStream::Stderr, b"\nc\n"),
        ];

        // Act
        let kept = tail_chunks(&chunks, 2);

        // Assert
        let seen: Vec<_> = kept.iter().map(|(c, b)| (c.stream, *b)).collect();
        assert_eq!(
            seen,
            vec![
                (AttachStream::Stdout, &b"b"[..]),
                (AttachStream::Stderr, &b"\nc\n"[..]),
            ]
        );
    }

    #[test]
    fn given_output_when_last_lines_taken_then_only_the_tail_remains() {
        // Arrange
        let output = b"a\nb\nc\n";

        // Act and Assert
        assert_eq!(last_lines(output, 2), b"b\nc\n");
        assert_eq!(last_lines(output, 9), output);
        assert_eq!(last_lines(output, 0), b"");
        assert_eq!(last_lines(b"a\nb", 1), b"b");
    }
}
