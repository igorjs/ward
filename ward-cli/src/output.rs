// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Output mode helpers shared by every subcommand.
//!
//! The CLI supports three rendering modes:
//!
//! 1. `--json` (or `WARD_JSON=1`): every command emits a single JSON
//!    object per result; list commands emit a JSON array. Stable for
//!    `jq` pipelines.
//! 2. Default + TTY + not piped: list commands use aligned tables
//!    (via `tabled`). Single-record commands keep `key: value` lines.
//! 3. Default + piped (or `--no-pretty`): tab-separated columns for
//!    list commands; key/value lines for single-record. Stable for
//!    `awk` / `cut` / `grep` in shell pipelines.
//!
//! The functions here are intentionally tiny so call sites stay
//! readable; per-command output stays in `main.rs`.

use std::io::IsTerminal;

use serde_json::Value;

/// True when stdout is an interactive TTY AND the user has not asked
/// for `--no-pretty`. List commands gate aligned-table rendering on
/// this; everything else stays unchanged.
pub fn pretty_tables_enabled(no_pretty: bool) -> bool {
    !no_pretty && std::io::stdout().is_terminal()
}

/// Emit a list result. `headers` names each column; `rows` carries the
/// values (one inner Vec per row, length must equal `headers.len()`).
/// In JSON mode: writes one array of `{header: value, ...}` objects.
/// In TTY mode (and `pretty_tables_enabled`): aligned table.
/// Otherwise: tab-separated lines (the historical default).
///
/// `json_values` lets callers pass typed values (numbers, bools, nulls)
/// rather than coercing everything to strings; the TTY / TSV paths
/// stringify via `Value::to_string` which preserves the natural
/// representation.
pub fn emit_rows(
    json: bool,
    no_pretty: bool,
    headers: &[&str],
    rows: &[Vec<Value>],
) -> anyhow::Result<()> {
    if json {
        let array: Vec<serde_json::Map<String, Value>> = rows
            .iter()
            .map(|row| {
                headers
                    .iter()
                    .zip(row.iter())
                    .map(|(h, v)| ((*h).to_string(), v.clone()))
                    .collect()
            })
            .collect();
        println!("{}", serde_json::to_string(&array)?);
        return Ok(());
    }

    if pretty_tables_enabled(no_pretty) {
        use tabled::{builder::Builder, settings::Style};
        let mut b = Builder::new();
        b.push_record(headers.iter().map(|h| h.to_string()));
        for row in rows {
            b.push_record(row.iter().map(stringify));
        }
        let mut t = b.build();
        t.with(Style::psql());
        println!("{t}");
        return Ok(());
    }

    // Tab-separated default. Empty output means "list found nothing"
    // (same convention as `grep`); scripts distinguish via exit code.
    for row in rows {
        let cols: Vec<String> = row.iter().map(stringify).collect();
        println!("{}", cols.join("\t"));
    }
    Ok(())
}

/// Stringify a `serde_json::Value` for human / TSV display. Strings
/// drop their surrounding quotes; everything else uses the natural
/// JSON representation (numbers as digits, bools as `true` / `false`,
/// null as the empty string for table cleanliness).
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn given_string_when_stringify_then_drops_quotes() {
        assert_eq!(stringify(&json!("hello")), "hello");
    }

    #[test]
    fn given_number_when_stringify_then_emits_digits() {
        assert_eq!(stringify(&json!(42)), "42");
    }

    #[test]
    fn given_bool_when_stringify_then_emits_keyword() {
        assert_eq!(stringify(&json!(true)), "true");
    }

    #[test]
    fn given_null_when_stringify_then_emits_empty_string() {
        // Empty cell renders cleaner in tables than a literal `null`.
        // Scripts can still detect absence via JSON mode if they need
        // to distinguish null from empty string.
        assert_eq!(stringify(&Value::Null), "");
    }
}
