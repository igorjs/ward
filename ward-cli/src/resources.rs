// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Docker-style `--memory` and `--cpus` values.

use anyhow::{Result, bail};

const KIB: u64 = 1024;
const MIB: u64 = KIB * 1024;
const GIB: u64 = MIB * 1024;
const TIB: u64 = GIB * 1024;

/// Parses a Docker memory size (`512m`, `1.5g`, `2GiB`, `1048576k`, plain
/// bytes) into whole MiB, rounding down. `0` means "daemon default" and is
/// passed through. Fails below 1 MiB or above what the limit holds.
pub fn parse_memory_mib(value: &str) -> Result<u32> {
    let lowered = value.trim().to_ascii_lowercase();
    // Docker accepts a trailing `b` and, after a unit letter, an `i`.
    let without_b = lowered.strip_suffix('b').unwrap_or(&lowered);
    let without_i = without_b
        .strip_suffix('i')
        .filter(|rest| rest.ends_with(['k', 'm', 'g', 't']))
        .unwrap_or(without_b);
    let (digits, unit) = match without_i.char_indices().last() {
        Some((i, 'k')) => (&without_i[..i], KIB),
        Some((i, 'm')) => (&without_i[..i], MIB),
        Some((i, 'g')) => (&without_i[..i], GIB),
        Some((i, 't')) => (&without_i[..i], TIB),
        _ => (without_i, 1),
    };
    let has_unit = digits.len() != lowered.len();
    let Some(count) = parse_decimal(digits) else {
        bail!("invalid memory size '{value}': use a number with an optional k, m, g or t suffix");
    };
    let bytes = count * unit as f64;
    if bytes == 0.0 {
        return Ok(0);
    }
    let mib = (bytes / MIB as f64).floor();
    if mib < 1.0 {
        let hint = if has_unit {
            String::new()
        } else {
            format!("; a number without a suffix is bytes, try '{value}m'")
        };
        bail!("memory limit '{value}' is below the 1 MiB minimum{hint}");
    }
    if mib > f64::from(u32::MAX) {
        bail!("memory limit '{value}' is too large");
    }
    Ok(mib as u32)
}

/// Parses a plain non-negative decimal such as `2` or `1.5`, rejecting signs,
/// exponents and `inf`/`nan` that `f64::from_str` would accept.
fn parse_decimal(digits: &str) -> Option<f64> {
    let valid = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.chars().any(|c| c.is_ascii_digit());
    valid.then(|| digits.parse().ok()).flatten()
}

/// Parses `--cpus`, which may be fractional, onto the whole CPU count the
/// daemon takes: rounded up. `0` means "daemon default" and is passed through.
pub fn parse_cpus(value: &str) -> Result<u32> {
    let cpus = value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|cpus| cpus.is_finite() && *cpus >= 0.0 && *cpus <= f64::from(u32::MAX))
        .ok_or_else(|| {
            anyhow::anyhow!("invalid CPU count '{value}': use a number such as 2 or 0.5")
        })?;
    Ok(cpus.ceil() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_docker_sizes_when_parsed_then_mib() {
        for (input, mib) in [
            ("512m", 512),
            ("2g", 2048),
            ("1048576k", 1024),
            ("1048576", 1),
            ("64MB", 64),
            ("1.5g", 1536),
            ("1.5m", 1),
            ("2mib", 2),
            ("1GiB", 1024),
            ("2048kib", 2),
            ("1t", 1024 * 1024),
            ("1048576b", 1),
            (".5g", 512),
        ] {
            assert_eq!(parse_memory_mib(input).unwrap(), mib, "{input}");
        }
    }

    #[test]
    fn given_zero_when_parsed_then_daemon_default_is_kept() {
        for input in ["0", "0m", "0.0g", "0b"] {
            assert_eq!(parse_memory_mib(input).unwrap(), 0, "{input}");
        }
    }

    #[test]
    fn given_below_one_mib_when_parsed_then_hint_only_for_unitless_input() {
        let bare = parse_memory_mib("512").unwrap_err().to_string();
        assert!(bare.contains("try '512m'"), "{bare}");
        for input in ["0.5m", "1023k", "512b"] {
            let err = parse_memory_mib(input).unwrap_err().to_string();
            assert!(err.contains("below the 1 MiB minimum"), "{err}");
            assert!(!err.contains("try"), "{err}");
        }
    }

    #[test]
    fn given_overflowing_size_when_parsed_then_error() {
        for input in ["99999999999999999999g", "4096t", "5000000t"] {
            assert!(parse_memory_mib(input).is_err(), "{input}");
        }
    }

    #[test]
    fn given_below_one_mib_or_garbage_when_parsed_then_error() {
        for input in [
            "512", "1023k", "abc", "m", "-1m", "", "1e3m", "inf", "nan", ".", "1.2.3m", "ib", "1ib",
        ] {
            assert!(parse_memory_mib(input).is_err(), "{input}");
        }
    }

    #[test]
    fn given_decimal_cpus_when_parsed_then_rounded_up_and_zero_kept() {
        for (input, cpus) in [("0.5", 1), ("1.5", 2), ("2", 2), ("0", 0), ("0.01", 1)] {
            assert_eq!(parse_cpus(input).unwrap(), cpus, "{input}");
        }
    }

    #[test]
    fn given_invalid_cpus_when_parsed_then_error() {
        for input in ["x", "-1", "NaN", ""] {
            assert!(parse_cpus(input).is_err(), "{input}");
        }
    }
}
