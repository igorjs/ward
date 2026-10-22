//! Guest process listing read from `/proc`, so no `ps` binary is needed.
//!
//! Times are shown in UTC: the guest has no local time zone to offer.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::proto::{ProcessRow, Processes};

/// Longest command a row carries, in bytes after sanitising.
const CMD_MAX_BYTES: usize = 4096;
/// Most rows a listing carries.
const MAX_ROWS: usize = 4096;
/// Most columns one `-o` list may name.
const MAX_COLUMNS: usize = 32;
/// Encoded size a listing stays under, well inside one agent frame.
const MAX_TABLE_BYTES: usize = crate::MAX_FRAME_BYTES as usize / 2;
/// Protobuf tag and length bytes counted per field against the budget.
const FIELD_OVERHEAD_BYTES: usize = 4;

/// Used when the kernel reports no clock tick rate.
const FALLBACK_TICKS_PER_SECOND: u64 = 100;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;
const SECONDS_PER_DAY: u64 = 86_400;
const BYTES_PER_KIB: u64 = 1024;
const EPOCH_YEAR: u64 = 1970;
const FEBRUARY: usize = 1;
const DAYS_IN_YEAR: u64 = 365;
const LEAP_YEARS: u64 = 4;
const CENTURY: u64 = 100;
const LEAP_CENTURIES: u64 = 400;
#[rustfmt::skip]
const MONTHS: [(&str, u64); 12] = [
    ("Jan", 31), ("Feb", 28), ("Mar", 31), ("Apr", 30), ("May", 31), ("Jun", 30),
    ("Jul", 31), ("Aug", 31), ("Sep", 30), ("Oct", 31), ("Nov", 30), ("Dec", 31),
];
/// `ps` caps the C column here and cuts longer user names to this width.
const MAX_C: u64 = 99;
const USER_WIDTH: usize = 8;
const USER_CUT_MARK: char = '+';

/// Fields of `/proc/<pid>/stat` after the closing parenthesis of the name.
const STAT_PPID: usize = 1;
const STAT_TTY: usize = 4;
const STAT_UTIME: usize = 11;
const STAT_STIME: usize = 12;
const STAT_START: usize = 19;
const STAT_VSIZE: usize = 20;
const STAT_RSS: usize = 21;
const STAT_FIELDS: usize = STAT_RSS + 1;
const STATE_ZOMBIE: &str = "Z";

/// Device numbers `ps` names: pseudo terminals and virtual consoles.
const TTY_MAJOR_PTS: u32 = 136;
const TTY_MAJOR_CONSOLE: u32 = 4;
const NO_TTY: &str = "?";

#[derive(Clone, Copy)]
#[rustfmt::skip]
enum Column {
    Uid, User, UserShort, Pid, Ppid, C, Stime, Tty, Time, Command, Comm, Stat, Vsz, Rss, Pcpu, Pmem,
}

type Layout = &'static [(&'static str, Column)];
#[rustfmt::skip]
const FULL: Layout = &[
    ("UID", Column::UserShort), ("PID", Column::Pid), ("PPID", Column::Ppid), ("C", Column::C),
    ("STIME", Column::Stime), ("TTY", Column::Tty), ("TIME", Column::Time), ("CMD", Column::Command),
];
/// `ps -e`.
#[rustfmt::skip]
const SHORT: Layout = &[
    ("PID", Column::Pid), ("TTY", Column::Tty), ("TIME", Column::Time), ("CMD", Column::Command),
];
/// `ps aux`.
#[rustfmt::skip]
const BSD: Layout = &[
    ("USER", Column::UserShort), ("PID", Column::Pid), ("%CPU", Column::Pcpu),
    ("%MEM", Column::Pmem), ("VSZ", Column::Vsz), ("RSS", Column::Rss), ("TTY", Column::Tty),
    ("STAT", Column::Stat), ("START", Column::Stime), ("TIME", Column::Time),
    ("COMMAND", Column::Command),
];

/// The columns `-o` accepts: name, header, column.
#[rustfmt::skip]
const OUTPUT_COLUMNS: [(&str, &str, Column); 16] = [
    ("pid", "PID", Column::Pid), ("ppid", "PPID", Column::Ppid), ("uid", "UID", Column::Uid),
    ("user", "USER", Column::User), ("comm", "COMMAND", Column::Comm),
    ("args", "COMMAND", Column::Command), ("cmd", "CMD", Column::Command),
    ("time", "TIME", Column::Time), ("stime", "STIME", Column::Stime), ("tty", "TT", Column::Tty),
    ("stat", "STAT", Column::Stat), ("vsz", "VSZ", Column::Vsz), ("rss", "RSS", Column::Rss),
    ("pcpu", "%CPU", Column::Pcpu), ("pmem", "%MEM", Column::Pmem), ("c", "C", Column::C),
];

const SUPPORTED_OPTIONS: &str =
    "aux, axu, -e, -A, -f, -ef, -Af, -o LIST, -eo LIST, -u USER, -p PID[,PID]";

fn unsupported(option: &str) -> String {
    let columns: Vec<&str> = OUTPUT_COLUMNS.iter().map(|c| c.0).collect();
    format!(
        "unsupported ps option: {option} (supported: {SUPPORTED_OPTIONS}; -o columns: {})",
        columns.join(", ")
    )
}

/// What a `ps` option list asks for: the columns and an optional filter.
pub struct Query {
    columns: Vec<(String, Column)>,
    user: Option<String>,
    pids: Vec<u32>,
}

impl Query {
    /// Read `ps` options. With none, the listing is `ps -ef`. The error
    /// names the first option that is not supported.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let (mut full, mut short, mut bsd) = (false, false, false);
        let (mut custom, mut user, mut pids) = (None, None, Vec::new());
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "aux" | "axu" => bsd = true,
                "-o" | "-eo" | "-Ao" => custom = Some(output_columns(next_value(&mut rest, arg)?)?),
                "-u" => user = Some(next_value(&mut rest, arg)?.clone()),
                "-p" => {
                    for pid in next_value(&mut rest, arg)?.split(',') {
                        pids.push(pid.parse().map_err(|_| unsupported(&format!("-p {pid}")))?);
                    }
                }
                flags => match flags.strip_prefix('-') {
                    Some(letters)
                        if !letters.is_empty() && letters.chars().all(|c| "eAf".contains(c)) =>
                    {
                        full |= letters.contains('f');
                        short = true;
                    }
                    _ => return Err(unsupported(flags)),
                },
            }
        }
        let layout = match (bsd, full || !short) {
            (true, _) => BSD,
            (false, true) => FULL,
            (false, false) => SHORT,
        };
        let columns = custom.unwrap_or_else(|| {
            layout
                .iter()
                .map(|(title, column)| (title.to_string(), *column))
                .collect()
        });
        Ok(Self {
            columns,
            user,
            pids,
        })
    }

    fn selects(&self, process: &Process, context: &Context) -> bool {
        let user_ok = self.user.as_ref().is_none_or(|wanted| {
            *wanted == context.user_name(process.owner) || *wanted == process.owner.to_string()
        });
        user_ok && (self.pids.is_empty() || self.pids.contains(&process.pid))
    }
}

fn next_value<'a>(
    rest: &mut std::slice::Iter<'a, String>,
    option: &str,
) -> Result<&'a String, String> {
    rest.next()
        .ok_or_else(|| format!("ps option {option} needs a value"))
}

fn output_columns(list: &str) -> Result<Vec<(String, Column)>, String> {
    let names: Vec<&str> = list
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() || names.len() > MAX_COLUMNS {
        return Err(unsupported(&format!("-o {list}")));
    }
    names
        .into_iter()
        .map(|name| {
            OUTPUT_COLUMNS
                .iter()
                .find(|(known, ..)| *known == name.to_ascii_lowercase())
                .map(|(_, title, column)| (title.to_string(), *column))
                .ok_or_else(|| unsupported(&format!("-o {name}")))
        })
        .collect()
}

/// One process as read from its `/proc/<pid>` directory.
struct Stat {
    comm: String,
    state: String,
    ppid: u32,
    tty: u32,
    cpu_ticks: u64,
    start_ticks: u64,
    vsize: u64,
    rss_pages: u64,
}

struct Process {
    pid: u32,
    owner: u32,
    stat: Stat,
    command: String,
}

/// Facts shared by every row of one listing.
struct Context {
    uptime: f64,
    boot_time: u64,
    ticks_per_second: u64,
    page_kib: u64,
    mem_total_kib: u64,
    users: HashMap<u32, String>,
}

impl Context {
    fn load(proc_root: &Path, passwd: &Path) -> std::io::Result<Self> {
        let ticks = rustix::param::clock_ticks_per_second();
        Ok(Self {
            uptime: first_number(&std::fs::read_to_string(proc_root.join("uptime"))?)?,
            boot_time: boot_time(&std::fs::read_to_string(proc_root.join("stat"))?)?,
            ticks_per_second: if ticks == 0 {
                FALLBACK_TICKS_PER_SECOND
            } else {
                ticks
            },
            page_kib: rustix::param::page_size() as u64 / BYTES_PER_KIB,
            mem_total_kib: mem_total_kib(&std::fs::read_to_string(proc_root.join("meminfo"))),
            users: user_map(&std::fs::read_to_string(passwd).unwrap_or_default()),
        })
    }

    /// The owner's name, or the number when `passwd` has no entry.
    fn user_name(&self, uid: u32) -> String {
        self.users
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| uid.to_string())
    }
}

/// List the processes under `proc_root` that `query` selects, lowest pid
/// first, naming owners from `passwd`. An entry that is not a process, or
/// that vanishes or cannot be parsed while it is read, is skipped. The
/// listing stops at the row and size caps.
pub fn list(proc_root: &Path, passwd: &Path, query: &Query) -> std::io::Result<Processes> {
    let context = Context::load(proc_root, passwd)?;
    let mut entries: Vec<(u32, PathBuf)> = std::fs::read_dir(proc_root)?
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path())))
        .collect();
    entries.sort_unstable();
    let mut rows = Vec::new();
    let mut bytes = 0;
    for (pid, dir) in entries {
        let Some(process) = read_process(pid, &dir) else {
            continue;
        };
        if !query.selects(&process, &context) {
            continue;
        }
        let fields: Vec<String> = query
            .columns
            .iter()
            .map(|(_, column)| process.field(*column, &context))
            .collect();
        bytes += fields
            .iter()
            .map(|f| f.len() + FIELD_OVERHEAD_BYTES)
            .sum::<usize>();
        if rows.len() >= MAX_ROWS || bytes > MAX_TABLE_BYTES {
            break;
        }
        rows.push(ProcessRow { fields });
    }
    Ok(Processes {
        titles: query
            .columns
            .iter()
            .map(|(title, _)| title.clone())
            .collect(),
        rows,
    })
}

fn read_process(pid: u32, dir: &Path) -> Option<Process> {
    let stat = read_stat(dir)?;
    let owner = std::fs::metadata(dir).ok()?.uid();
    let mut cmdline = Vec::new();
    if let Ok(file) = std::fs::File::open(dir.join("cmdline")) {
        let _ = file.take(CMD_MAX_BYTES as u64).read_to_end(&mut cmdline);
    }
    // A different start time means the pid was reused while it was read.
    let unchanged = read_stat(dir)?.start_ticks == stat.start_ticks;
    unchanged.then(|| Process {
        pid,
        owner,
        command: command(&stat, &cmdline),
        stat,
    })
}

fn read_stat(dir: &Path) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(dir.join("stat")).ok()?)
}

/// The name sits in parentheses and may itself hold spaces and parentheses,
/// so the numeric fields start after the last closing parenthesis.
fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = printable(text.get(open + 1..close)?);
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    if rest.len() < STAT_FIELDS {
        return None;
    }
    let number = |index: usize| rest[index].parse::<u64>().ok();
    Some(Stat {
        comm,
        state: rest[0].to_string(),
        ppid: u32::try_from(number(STAT_PPID)?).ok()?,
        tty: u32::try_from(rest[STAT_TTY].parse::<i64>().ok()?.max(0)).ok()?,
        cpu_ticks: number(STAT_UTIME)? + number(STAT_STIME)?,
        start_ticks: number(STAT_START)?,
        vsize: number(STAT_VSIZE)?,
        rss_pages: number(STAT_RSS)?,
    })
}

impl Process {
    fn field(&self, column: Column, context: &Context) -> String {
        let stat = &self.stat;
        let rss_kib = stat.rss_pages * context.page_kib;
        match column {
            Column::Uid => self.owner.to_string(),
            Column::User => context.user_name(self.owner),
            Column::UserShort => short_user(&context.user_name(self.owner)),
            Column::Pid => self.pid.to_string(),
            Column::Ppid => stat.ppid.to_string(),
            Column::C => (self.cpu_percent(context).round() as u64)
                .min(MAX_C)
                .to_string(),
            Column::Stime => self.start_time(context),
            Column::Tty => tty_name(stat.tty),
            Column::Time => duration(stat.cpu_ticks / context.ticks_per_second),
            Column::Command => self.command.clone(),
            Column::Comm => stat.comm.clone(),
            Column::Stat => stat.state.clone(),
            Column::Vsz => (stat.vsize / BYTES_PER_KIB).to_string(),
            Column::Rss => rss_kib.to_string(),
            Column::Pcpu => format!("{:.1}", self.cpu_percent(context)),
            Column::Pmem => {
                let share = if context.mem_total_kib == 0 {
                    0.0
                } else {
                    100.0 * rss_kib as f64 / context.mem_total_kib as f64
                };
                format!("{share:.1}")
            }
        }
    }

    /// CPU time as a share of the time since the process started, like `ps`.
    fn cpu_percent(&self, context: &Context) -> f64 {
        let ticks = context.ticks_per_second as f64;
        let elapsed = context.uptime - self.stat.start_ticks as f64 / ticks;
        if elapsed <= 0.0 {
            return 0.0;
        }
        100.0 * (self.stat.cpu_ticks as f64 / ticks) / elapsed
    }

    /// `HH:MM`, or `MonDD` once the process is a day old, both in UTC.
    fn start_time(&self, context: &Context) -> String {
        let started = self.stat.start_ticks / context.ticks_per_second;
        let epoch = context.boot_time + started;
        if context.uptime - started as f64 >= SECONDS_PER_DAY as f64 {
            month_day(epoch)
        } else {
            clock(epoch)
        }
    }
}

fn first_number(text: &str) -> std::io::Result<f64> {
    text.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| invalid("uptime is not a number"))
}

fn boot_time(stat: &str) -> std::io::Result<u64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| invalid("boot time is missing"))
}

/// Total memory in KiB, or 0 when `/proc/meminfo` cannot say.
fn mem_total_kib(meminfo: &std::io::Result<String>) -> u64 {
    meminfo
        .as_ref()
        .ok()
        .and_then(|text| text.lines().find_map(|l| l.strip_prefix("MemTotal:")))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// User ids to names from `passwd`; the first entry for an id wins.
fn user_map(passwd: &str) -> HashMap<u32, String> {
    let mut users = HashMap::new();
    for line in passwd.lines() {
        let mut parts = line.split(':');
        if let (Some(name), Some(Ok(uid))) = (parts.next(), parts.nth(1).map(str::parse)) {
            users.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    users
}

/// A long name cut to the width `ps` gives it, ending in a mark.
fn short_user(name: &str) -> String {
    if name.chars().count() <= USER_WIDTH {
        return name.to_string();
    }
    name.chars()
        .take(USER_WIDTH - 1)
        .chain([USER_CUT_MARK])
        .collect()
}

/// Wall-clock `HH:MM` (UTC) of an epoch second.
fn clock(epoch_seconds: u64) -> String {
    let day = epoch_seconds % SECONDS_PER_DAY;
    format!(
        "{:02}:{:02}",
        day / SECONDS_PER_HOUR,
        day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE
    )
}

/// The UTC date of an epoch second as `MonDD`.
fn month_day(epoch_seconds: u64) -> String {
    let mut day = epoch_seconds / SECONDS_PER_DAY;
    let mut year = EPOCH_YEAR;
    while day >= year_length(year) {
        day -= year_length(year);
        year += 1;
    }
    for (index, (name, _)) in MONTHS.iter().enumerate() {
        let length = month_length(year, index);
        if day < length || index == MONTHS.len() - 1 {
            return format!("{name}{:02}", day + 1);
        }
        day -= length;
    }
    unreachable!("the last month always returns")
}

fn is_leap(year: u64) -> bool {
    year.is_multiple_of(LEAP_YEARS)
        && (!year.is_multiple_of(CENTURY) || year.is_multiple_of(LEAP_CENTURIES))
}

fn month_length(year: u64, month: usize) -> u64 {
    MONTHS[month].1 + u64::from(month == FEBRUARY && is_leap(year))
}

fn year_length(year: u64) -> u64 {
    DAYS_IN_YEAR + u64::from(is_leap(year))
}

fn duration(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / SECONDS_PER_HOUR,
        seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
        seconds % SECONDS_PER_MINUTE
    )
}

/// Decode a Linux device number the way `ps` does.
fn tty_name(device: u32) -> String {
    let major = device >> 8 & 0xfff;
    let minor = device & 0xff | device >> 12 & 0xfff00;
    match major {
        TTY_MAJOR_PTS => format!("pts/{minor}"),
        TTY_MAJOR_CONSOLE => format!("tty{minor}"),
        _ => NO_TTY.to_string(),
    }
}

/// Text safe to print: control characters become `?`, and the NULs that
/// separate arguments become spaces.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\0' => ' ',
            c if c.is_control() => '?',
            c => c,
        })
        .collect()
}

/// The command line, or the bracketed name for a process that has none
/// (kernel threads and zombies, which `ps` marks as defunct). Empty
/// arguments are kept.
fn command(stat: &Stat, cmdline: &[u8]) -> String {
    let zombie = stat.state == STATE_ZOMBIE;
    if cmdline.is_empty() || zombie {
        let defunct = if zombie { " <defunct>" } else { "" };
        return format!("[{}]{defunct}", stat.comm);
    }
    let body = cmdline.strip_suffix(&[0]).unwrap_or(cmdline);
    let mut text = printable(&String::from_utf8_lossy(body));
    let mut end = text.len().min(CMD_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const BOOT_TIME: u64 = 3600;
    const UPTIME_SECONDS: u64 = 1000;
    /// Kernel clock rate the fixtures are written for.
    const TICKS: u64 = 100;
    const START_AFTER_BOOT_TICKS: u64 = 100 * TICKS;
    const FULL_TITLES: [&str; 8] = ["UID", "PID", "PPID", "C", "STIME", "TTY", "TIME", "CMD"];
    const BSD_TITLES: [&str; 11] = [
        "USER", "PID", "%CPU", "%MEM", "VSZ", "RSS", "TTY", "STAT", "START", "TIME", "COMMAND",
    ];
    const COL_PID: usize = 1;
    const COL_PPID: usize = 2;
    const COL_C: usize = 3;
    const COL_STIME: usize = 4;
    const COL_TTY: usize = 5;
    const COL_TIME: usize = 6;
    const COL_CMD: usize = 7;
    const SOLE_PID: u32 = 5;
    const VALID_STAT: &str = "13 (ok) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0";
    /// Longer than the eight characters the full form shows.
    const OWNER_NAME: &str = "averylongusername";
    const OWNER_SHOWN: &str = "averylo+";
    const PTS_MAJOR: u32 = 136;
    const CONSOLE_MAJOR: u32 = 4;
    const MAJOR_SHIFT: u32 = 8;
    const VSIZE_BYTES: u64 = 8192;
    const RSS_PAGES: u64 = 3;
    const BIG_PROCESSES: u32 = 600;
    const BIG_CMDLINE_BYTES: usize = 10_000;

    /// A process dir with a stat line and a NUL-separated command line.
    fn add_process(root: &Path, pid: &str, stat: &str, cmdline: &[u8]) {
        let dir = root.join(pid);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stat"), stat).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    /// A `/proc/<pid>/stat` line: pid, comm, state, ppid, tty, cpu ticks, start ticks.
    fn stat_line(
        pid: u32,
        comm: &str,
        state: char,
        ppid: u32,
        tty: u32,
        ticks: u64,
        start: u64,
    ) -> String {
        let (utime, stime) = (ticks / 2, ticks - ticks / 2);
        format!(
            "{pid} ({comm}) {state} {ppid} 1 1 {tty} -1 4194560 0 0 0 0 {utime} {stime} 0 0 20 0 1 0 {start} {VSIZE_BYTES} {RSS_PAGES}"
        )
    }

    fn fake_proc() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proc");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("uptime"), format!("{UPTIME_SECONDS}.50 900.00\n")).unwrap();
        fs::write(root.join("stat"), format!("cpu 1 2 3\nbtime {BOOT_TIME}\n")).unwrap();
        let passwd = dir.path().join("passwd");
        let owner = fs::metadata(&root).unwrap().uid();
        fs::write(
            &passwd,
            format!("root:x:0:0:root:/root:/bin/sh\n{OWNER_NAME}:x:{owner}:{owner}::/:/bin/sh\n"),
        )
        .unwrap();
        (dir, passwd)
    }

    fn test_context() -> Context {
        Context {
            uptime: UPTIME_SECONDS as f64,
            boot_time: BOOT_TIME,
            ticks_per_second: TICKS,
            page_kib: 1,
            mem_total_kib: 0,
            users: HashMap::new(),
        }
    }

    fn query(args: &[&str]) -> Query {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Query::parse(&args).unwrap()
    }

    fn list_all(root: &Path, passwd: &Path) -> std::io::Result<Processes> {
        list(root, passwd, &query(&[]))
    }

    fn rows(list: &Processes) -> Vec<Vec<String>> {
        list.rows.iter().map(|r| r.fields.clone()).collect()
    }

    /// The only row of a fake proc holding one process, listed with `args`.
    fn one(stat: &str, cmdline: &[u8], args: &[&str]) -> Vec<String> {
        let (dir, passwd) = fake_proc();
        let root = dir.path().join("proc");
        add_process(&root, &SOLE_PID.to_string(), stat, cmdline);
        list(&root, &passwd, &query(args))
            .unwrap()
            .rows
            .remove(0)
            .fields
    }

    #[test]
    fn given_a_running_process_when_listed_then_its_row_follows_ps_ef() {
        // Arrange: started 100 s after boot, so 900 s ago; 90 s of cpu is 10 percent.
        let stat = stat_line(SOLE_PID, "sleep", 'S', 1, 0, 9000, START_AFTER_BOOT_TICKS);

        // Act
        let row = one(&stat, b"sleep\x00600\x00", &[]);

        // Assert
        assert_eq!(row[0], OWNER_SHOWN);
        assert_eq!(row[COL_PID], SOLE_PID.to_string());
        assert_eq!(row[COL_PPID], "1");
        assert_eq!(row[COL_C], "10");
        assert_eq!(row[COL_STIME], "01:01");
        assert_eq!(row[COL_TTY], "?");
        assert_eq!(row[COL_TIME], "00:01:30");
        assert_eq!(row[COL_CMD], "sleep 600");
    }

    #[rstest::rstest]
    #[case::spaces_and_parens("a) (b c", 'S', b"", "[a) (b c]")]
    #[case::zombie("gone", 'Z', b"", "[gone] <defunct>")]
    #[case::kernel_thread("kthreadd", 'S', b"", "[kthreadd]")]
    #[case::control_in_comm("k\tthread", 'S', b"", "[k?thread]")]
    #[case::control_in_argv(
        "sh",
        'S',
        b"sh\0a\x1b[31mb\0l1\nl2\0del\x7f\0",
        "sh a?[31mb l1?l2 del?"
    )]
    #[case::empty_args_kept("sh", 'S', b"sh\0\0x\0\0", "sh  x ")]
    fn given_a_process_when_listed_then_its_command_reads_like_ps(
        #[case] comm: &str,
        #[case] state: char,
        #[case] cmdline: &[u8],
        #[case] expected: &str,
    ) {
        // Arrange
        let stat = stat_line(SOLE_PID, comm, state, 4, 0, 0, 0);

        // Act
        let row = one(&stat, cmdline, &[]);

        // Assert
        assert_eq!(row[COL_PPID], "4");
        assert_eq!(row[COL_CMD], expected);
    }

    #[rstest::rstest]
    #[case::not_a_number("self", VALID_STAT)]
    #[case::mixed("12abc", VALID_STAT)]
    #[case::sys("sys", VALID_STAT)]
    #[case::empty("12", "")]
    #[case::no_paren("12", "12 S 1")]
    #[case::truncated("12", "12 (x) S 1 1")]
    #[case::bad_number("12", "12 (x) S one 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0")]
    fn given_an_entry_that_is_not_a_readable_process_when_listed_then_it_is_skipped(
        #[case] name: &str,
        #[case] stat: &str,
    ) {
        // Arrange: a vanished process (no stat file) is skipped the same way.
        let (dir, passwd) = fake_proc();
        let root = dir.path().join("proc");
        add_process(&root, name, stat, b"x\0");
        add_process(&root, "13", VALID_STAT, b"ok\0");
        fs::create_dir_all(root.join("30")).unwrap();

        // Act
        let listed = rows(&list_all(&root, &passwd).unwrap());

        // Assert
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0][COL_PID], "13");
    }

    #[rstest::rstest]
    #[case::pts(PTS_MAJOR << MAJOR_SHIFT | 3, "pts/3")]
    #[case::console(CONSOLE_MAJOR << MAJOR_SHIFT | 1, "tty1")]
    #[case::none(0, "?")]
    #[case::unknown(7 << MAJOR_SHIFT, "?")]
    fn given_a_tty_number_when_named_then_it_reads_like_ps(#[case] tty: u32, #[case] name: &str) {
        // Arrange / Act
        let shown = tty_name(tty);

        // Assert
        assert_eq!(shown, name);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn given_a_spawned_sleep_when_listed_from_real_proc_then_one_row_has_its_pid_and_command() {
        // Arrange
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();

        // Act
        let listed = list_all(Path::new("/proc"), Path::new("/etc/passwd")).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        // Assert
        let pid = child.id().to_string();
        let found: Vec<_> = rows(&listed)
            .into_iter()
            .filter(|r| r[COL_PID] == pid)
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0][COL_CMD], "sleep 600");
    }

    #[rstest::rstest]
    #[case::none(&[], &FULL_TITLES[..])]
    #[case::all(&["-e"], &["PID", "TTY", "TIME", "CMD"][..])]
    #[case::aux(&["aux"], &BSD_TITLES[..])]
    #[case::columns(&["-eo", "pid,comm"], &["PID", "COMMAND"][..])]
    fn given_ps_options_when_parsed_then_titles_follow_ps(
        #[case] args: &[&str],
        #[case] titles: &[&str],
    ) {
        // Arrange
        let (dir, passwd) = fake_proc();

        // Act
        let listed = list(&dir.path().join("proc"), &passwd, &query(args)).unwrap();

        // Assert
        assert_eq!(listed.titles, titles);
    }

    #[rstest::rstest]
    #[case::sort(&["--sort=pid"], "unsupported ps option: --sort=pid (")]
    #[case::unknown_flag(&["-x"], "unsupported ps option: -x (")]
    #[case::bad_column(&["-o", "pid,nope"], "unsupported ps option: -o nope (")]
    #[case::bad_pid(&["-p", "1,x"], "unsupported ps option: -p x (")]
    #[case::missing_value(&["-u"], "ps option -u needs a value")]
    fn given_an_unsupported_ps_option_when_parsed_then_it_is_named(
        #[case] args: &[&str],
        #[case] expected: &str,
    ) {
        // Arrange
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();

        // Act
        let error = Query::parse(&args).err().expect("refused");

        // Assert
        assert!(error.starts_with(expected), "{error}");
        if expected.starts_with("unsupported") {
            assert!(error.contains(SUPPORTED_OPTIONS), "{error}");
        }
    }

    #[test]
    fn given_every_output_column_when_listed_then_each_value_follows_ps() {
        // Arrange
        let uid = rustix::process::getuid().as_raw().to_string();
        // Five times the elapsed cpu time, so pcpu passes 100 and c is capped.
        let stat = stat_line(SOLE_PID, "worker", 'S', 2, 0, UPTIME_SECONDS * TICKS * 5, 0);
        let columns = "pid,ppid,uid,user,comm,args,cmd,time,stime,tty,stat,vsz,rss,pcpu,pmem,c";

        // Act
        let row = one(&stat, b"worker\0-x\0", &["-o", columns]);

        // Assert: the owner of the fake proc directory is the current user.
        let rss_kib = RSS_PAGES * rustix::param::page_size() as u64 / BYTES_PER_KIB;
        let vsz_kib = VSIZE_BYTES / BYTES_PER_KIB;
        let expected = format!(
            "{SOLE_PID} 2 {uid} {OWNER_NAME} worker worker -x worker -x 01:23:20 01:00 ? S \
             {vsz_kib} {rss_kib} 499.8 0.0 99"
        );
        assert_eq!(row.join(" "), expected);
    }

    #[test]
    fn given_a_user_or_pid_filter_when_listed_then_only_matching_processes_remain() {
        // Arrange
        let (dir, passwd) = fake_proc();
        let root = dir.path().join("proc");
        for pid in [10, 11] {
            let stat = stat_line(pid, "x", 'S', 1, 0, 0, 0);
            add_process(&root, &pid.to_string(), &stat, b"x\0");
        }

        // Act
        let pids = |args: &[&str]| -> Vec<String> {
            let listed = list(&root, &passwd, &query(args)).unwrap();
            listed
                .rows
                .into_iter()
                .map(|r| r.fields[0].clone())
                .collect()
        };

        // Assert
        assert_eq!(pids(&["-o", "pid", "-p", "11"]), ["11"]);
        assert_eq!(pids(&["-o", "pid", "-u", OWNER_NAME]), ["10", "11"]);
        assert!(pids(&["-o", "pid", "-u", "root"]).is_empty());
    }

    #[rstest::rstest]
    #[case::epoch_zero(0, "Jan01")]
    #[case::leap_day(1_709_164_800, "Feb29")]
    #[case::after_leap_day(1_709_251_200, "Mar01")]
    fn given_an_epoch_when_formatted_then_it_is_the_utc_month_and_day(
        #[case] epoch: u64,
        #[case] expected: &str,
    ) {
        // Arrange / Act
        let text = month_day(epoch);

        // Assert
        assert_eq!(text, expected);
    }

    #[rstest::rstest]
    #[case::recent(UPTIME_SECONDS, "01:01")]
    #[case::a_day_old(SECONDS_PER_DAY + UPTIME_SECONDS, "Jan01")]
    fn given_a_process_age_when_its_start_is_shown_then_old_ones_get_a_date(
        #[case] uptime: u64,
        #[case] expected: &str,
    ) {
        // Arrange
        let context = Context {
            uptime: uptime as f64,
            ..test_context()
        };
        let stat = stat_line(1, "x", 'S', 0, 0, 0, START_AFTER_BOOT_TICKS);
        let process = Process {
            pid: 1,
            owner: 0,
            command: String::new(),
            stat: parse_stat(&stat).unwrap(),
        };

        // Act
        let shown = process.start_time(&context);

        // Assert
        assert_eq!(shown, expected);
    }

    #[rstest::rstest]
    #[case::ascii("a")]
    #[case::multibyte("\u{e9}")]
    fn given_many_huge_command_lines_when_listed_then_each_is_cut_and_the_table_fits_a_frame(
        #[case] unit: &str,
    ) {
        // Arrange
        let (dir, passwd) = fake_proc();
        let root = dir.path().join("proc");
        let cmdline = unit.repeat(BIG_CMDLINE_BYTES).into_bytes();
        for pid in 1..=BIG_PROCESSES {
            let stat = stat_line(pid, "big", 'S', 1, 0, 0, 0);
            add_process(&root, &pid.to_string(), &stat, &cmdline);
        }

        // Act
        let listed = list_all(&root, &passwd).unwrap();

        // Assert
        assert!(listed.rows.len() < BIG_PROCESSES as usize);
        assert!(
            listed
                .rows
                .iter()
                .all(|r| r.fields[COL_CMD].len() > CMD_MAX_BYTES - unit.len())
        );
        let event = crate::proto::Event {
            kind: Some(crate::proto::event::Kind::Processes(listed)),
        };
        assert!(prost::Message::encoded_len(&event) < crate::MAX_FRAME_BYTES as usize);
    }
}
