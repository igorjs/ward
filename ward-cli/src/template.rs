// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The small part of Go templates that Docker's `--format` flags use:
//! `{{.Field}}`, `{{json .}}`, the `\t` and `\n` escapes, and a `table `
//! prefix that adds a header row and aligns the columns.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

const TABLE_PREFIX: &str = "table ";
/// Docker accepts the bare word as shorthand for `{{json .}}`.
const JSON_SHORTHAND: &str = "json";
const JSON_TEMPLATE: &str = "{{json .}}";
const ACTION_OPEN: &str = "{{";
const ACTION_CLOSE: &str = "}}";
const FIELD_PREFIX: &str = ".";
const JSON_ACTION: &str = "json .";
const COLUMN_SEPARATOR: char = '\t';
const COLUMN_PADDING: usize = 3;

enum Part {
    Text(String),
    Field { name: String, title: &'static str },
    Json,
}

/// A parsed `--format` template.
pub struct Template {
    table: bool,
    parts: Vec<Part>,
}

impl Template {
    /// Parses `format`. `columns` pairs each field name it may use with the
    /// header title shown above it in a table.
    ///
    /// # Errors
    ///
    /// Fails on a field not in `columns`, an action other than a field or
    /// `json .`, or an action that is never closed.
    pub fn parse(format: &str, columns: &[(&str, &'static str)]) -> Result<Self> {
        let (table, body) = match format.strip_prefix(TABLE_PREFIX) {
            Some(body) => (true, body),
            None => (false, format),
        };
        let body = if body == JSON_SHORTHAND {
            JSON_TEMPLATE
        } else {
            body
        };
        let body = body.replace("\\t", "\t").replace("\\n", "\n");
        let mut parts = Vec::new();
        let mut rest = body.as_str();
        while let Some((text, tail)) = rest.split_once(ACTION_OPEN) {
            let (action, after) = tail
                .split_once(ACTION_CLOSE)
                .ok_or_else(|| anyhow!("template {format:?} has an unclosed {ACTION_OPEN}"))?;
            parts.push(Part::Text(text.to_string()));
            parts.push(Self::action(action.trim(), columns)?);
            rest = after;
        }
        parts.push(Part::Text(rest.to_string()));
        Ok(Self { table, parts })
    }

    fn action(action: &str, columns: &[(&str, &'static str)]) -> Result<Part> {
        if action == JSON_ACTION {
            return Ok(Part::Json);
        }
        let Some(name) = action.strip_prefix(FIELD_PREFIX) else {
            bail!("unsupported action {{{{{action}}}}} in --format");
        };
        match columns.iter().find(|(field, _)| names_field(field, name)) {
            Some((_, title)) => Ok(Part::Field {
                name: name.to_string(),
                title,
            }),
            None => bail!("unknown field .{name} in --format"),
        }
    }

    /// One line per record, with a header row and aligned columns for a
    /// `table ` template. `None` renders the header.
    fn line(&self, record: Option<&Map<String, Value>>) -> String {
        let cell = |part: &Part| match (part, record) {
            (Part::Text(text), _) => text.clone(),
            (Part::Field { title, .. }, None) => (*title).to_string(),
            (Part::Field { name, .. }, Some(r)) => match r.get(name) {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
            (Part::Json, None) => String::new(),
            (Part::Json, Some(r)) => Value::Object(r.clone()).to_string(),
        };
        self.parts.iter().map(cell).collect()
    }

    pub fn render(&self, records: &[Map<String, Value>]) -> String {
        if !self.table {
            return records.iter().map(|r| self.line(Some(r)) + "\n").collect();
        }
        let lines: Vec<Vec<String>> = std::iter::once(self.line(None))
            .chain(records.iter().map(|r| self.line(Some(r))))
            .map(|l| l.split(COLUMN_SEPARATOR).map(String::from).collect())
            .collect();
        align(&lines)
    }
}

/// A column ending in `.` stands for every key under it, such as
/// `Actor.Attributes.` for `Actor.Attributes.name`.
fn names_field(column: &str, name: &str) -> bool {
    column == name || (column.ends_with('.') && name.starts_with(column))
}

/// Lines of cells as columns padded to their longest cell. The last column
/// is left unpadded.
fn align(lines: &[Vec<String>]) -> String {
    let mut out = String::new();
    for line in lines {
        for (column, cell) in line.iter().enumerate() {
            out.push_str(cell);
            if column + 1 < line.len() {
                let widest = lines
                    .iter()
                    .filter_map(|l| l.get(column))
                    .map(|c| c.chars().count());
                let pad = widest.max().unwrap_or(0) + COLUMN_PADDING - cell.chars().count();
                out.push_str(&" ".repeat(pad));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COLUMNS: [(&str, &str); 2] = [("Name", "NAME"), ("PIDs", "PIDS")];

    fn records() -> Vec<Map<String, Value>> {
        let object = |name: &str, pids: u32| match json!({"Name": name, "PIDs": pids}) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        vec![object("web1", 3), object("db", 12)]
    }

    #[rstest::rstest]
    #[case::fields("{{.Name}}:{{ .PIDs }}", "web1:3\ndb:12\n")]
    #[case::escapes("{{.Name}}\\t{{.PIDs}}\\n", "web1\t3\n\ndb\t12\n\n")]
    #[case::json_action(
        "{{json .}}",
        "{\"Name\":\"web1\",\"PIDs\":3}\n{\"Name\":\"db\",\"PIDs\":12}\n"
    )]
    #[case::table("table {{.Name}}\\t{{.PIDs}}", "NAME   PIDS\nweb1   3\ndb     12\n")]
    fn given_a_template_when_rendered_then_each_record_is_substituted(
        #[case] format: &str,
        #[case] expected: &str,
    ) {
        let template = Template::parse(format, &COLUMNS).expect("parse");

        assert_eq!(template.render(&records()), expected);
    }

    #[rstest::rstest]
    #[case::unknown_field("{{.Nope}}", ".Nope")]
    #[case::other_action("{{printf}}", "printf")]
    fn given_a_bad_template_when_parsed_then_the_error_names_the_problem(
        #[case] format: &str,
        #[case] named: &str,
    ) {
        let error = Template::parse(format, &COLUMNS).err().expect("rejected");

        assert!(error.to_string().contains(named), "{error}");
    }
}
