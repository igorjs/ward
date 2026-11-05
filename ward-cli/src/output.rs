// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Output mode helpers shared by every subcommand.
//!
//! The CLI supports three rendering modes:
//!
//! 1. `--json` (or `WARD_JSON=1`): every command emits a single JSON
//!    object per result; list commands emit a JSON array. Stable for
//!    `jq` pipelines.
//! 2. Default: list commands print Docker's space-aligned table with a
//!    header row, whether or not stdout is a TTY.
//! 3. `--no-pretty` (or `WARD_NO_PRETTY=1`): tab-separated columns for
//!    list commands, for `cut -f` pipelines. This one is Ward only.
//!
//! The functions here are intentionally tiny so call sites stay
//! readable; per-command output stays in `main.rs`.

use serde_json::Value;

/// Header line for tab-separated output. Aligned tables carry their own
/// header, so they return `None`.
pub fn tsv_header(no_pretty: bool, headers: &[&str]) -> Option<String> {
    no_pretty.then(|| headers.join("\t"))
}

/// Emit a list result. `headers` names each column; `rows` carries the
/// values (one inner Vec per row, length must equal `headers.len()`).
/// In JSON mode: writes one array of `{header: value, ...}` objects.
/// Otherwise: Docker's aligned table, or tab-separated lines with `no_pretty`.
///
/// Table and TSV paths stringify values via `Value::to_string`, which keeps
/// the natural representation.
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

    if !no_pretty {
        let mut lines = vec![headers.iter().map(|h| h.to_uppercase()).collect()];
        lines.extend(rows.iter().map(|row| row.iter().map(stringify).collect()));
        print!("{}", crate::template::align(&lines));
        return Ok(());
    }

    // Empty output means "list found nothing"
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
    fn given_default_mode_when_header_built_then_none_because_the_table_has_one() {
        assert_eq!(tsv_header(false, &["A", "B"]), None);
    }

    #[test]
    fn given_no_pretty_when_header_built_then_columns_are_tab_separated() {
        assert_eq!(tsv_header(true, &["A", "B"]).as_deref(), Some("A\tB"));
    }

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
