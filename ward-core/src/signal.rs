// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Linux signal names and numbers, as `docker kill -s` and `--stop-signal`
//! spell them.

pub const SIGKILL: i32 = 9;
pub const SIGTERM: i32 = 15;
/// Highest signal number Linux accepts (the last real-time signal).
pub const MAX_SIGNAL: i32 = 64;

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
/// Accept `TERM`, `SIGTERM`, `term` or a number from 1 to 64.
#[must_use]
pub fn parse(text: &str) -> Option<i32> {
    if let Ok(number) = text.parse::<i32>() {
        return (1..=MAX_SIGNAL).contains(&number).then_some(number);
    }
    let name = text.to_ascii_uppercase();
    let name = name.strip_prefix("SIG").unwrap_or(&name);
    SIGNALS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, number)| *number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::bare("TERM", Some(SIGTERM))]
    #[case::prefixed("SIGKILL", Some(SIGKILL))]
    #[case::lower("sigint", Some(2))]
    #[case::number("10", Some(10))]
    #[case::zero("0", None)]
    #[case::past_max("65", None)]
    #[case::unknown("NOPE", None)]
    fn given_signal_text_when_parsed_then_number_or_none(
        #[case] text: &str,
        #[case] want: Option<i32>,
    ) {
        assert_eq!(parse(text), want);
    }
}
