// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward top SANDBOX [ps OPTIONS]`, laid out like `docker top`.

use std::path::Path;

use anyhow::Result;
use serde_json::json;
use ward_core::pb::TopRequest;

use crate::client;

/// Docker's tab writer pads a column to at least this width.
const MIN_COLUMN_WIDTH: usize = 20;
/// Docker's tab writer separates columns by at least this many spaces.
const COLUMN_PADDING: usize = 3;

/// Lists the processes of `reference` and prints them.
pub async fn run(
    socket: &str,
    token_store: &Path,
    reference: &str,
    ps_args: Vec<String>,
    json: bool,
) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let request = target.request(TopRequest {
        sandbox_id: target.id.clone(),
        ps_args,
    })?;
    let reply = c.top(request).await?.into_inner();
    let rows: Vec<Vec<String>> = reply.processes.into_iter().map(|p| p.fields).collect();
    if json {
        println!("{}", json!({"Titles": reply.titles, "Processes": rows}));
    } else {
        print!("{}", render(&reply.titles, &rows));
    }
    Ok(())
}

/// Columns aligned as `docker top` aligns them. The last column is not padded.
fn render(titles: &[String], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..titles.len())
        .map(|column| {
            let longest = std::iter::once(titles.get(column))
                .chain(rows.iter().map(|row| row.get(column)))
                .flatten()
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0);
            (longest + COLUMN_PADDING).max(MIN_COLUMN_WIDTH)
        })
        .collect();
    let mut out = String::new();
    for line in std::iter::once(titles).chain(rows.iter().map(Vec::as_slice)) {
        for (column, cell) in line.iter().enumerate() {
            out.push_str(cell);
            if column + 1 < line.len() {
                let pad = widths[column].saturating_sub(cell.chars().count());
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

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn given_a_long_cell_when_rendered_then_the_column_grows_and_the_last_is_unpadded() {
        // Arrange
        let long = "x".repeat(MIN_COLUMN_WIDTH + 5);
        let titles = strings(&["A", "B"]);
        let rows = vec![vec![long.clone(), "end".to_string()]];

        // Act
        let text = render(&titles, &rows);

        // Assert
        let lines: Vec<&str> = text.lines().collect();
        let width = long.len() + COLUMN_PADDING;
        assert_eq!(lines[0], format!("A{}B", " ".repeat(width - 1)));
        assert_eq!(lines[1], format!("{long}{}end", " ".repeat(COLUMN_PADDING)));
    }
}
