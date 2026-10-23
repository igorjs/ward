// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `ward diff SANDBOX`: paths that differ from the image, one per line as
//! `docker diff` prints them.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use ward_core::pb::{ChangeKind, DiffRequest, FileChange};

use crate::client;

/// Lists the changes of `reference` against its image and prints them.
pub async fn run(socket: &str, token_store: &Path, reference: &str, json: bool) -> Result<()> {
    let mut c = client::connect(socket).await?;
    let target = client::resolve(&mut c, token_store, reference).await?;
    let request = target.request(DiffRequest {
        sandbox_id: target.id.clone(),
    })?;
    let changes = c.diff(request).await?.into_inner().changes;
    if json {
        println!("{}", Value::Array(changes.iter().map(as_json).collect()));
    } else {
        print!("{}", render(&changes));
    }
    Ok(())
}

/// Docker's numeric kinds in `--json` output.
const JSON_KIND_CHANGED: u8 = 0;
const JSON_KIND_ADDED: u8 = 1;
const JSON_KIND_DELETED: u8 = 2;

/// Docker's letter for a change: `C`, `A` or `D`.
fn letter(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "A",
        ChangeKind::Deleted => "D",
        ChangeKind::Modified | ChangeKind::Unspecified => "C",
    }
}

fn render(changes: &[FileChange]) -> String {
    changes
        .iter()
        .map(|change| format!("{} {}\n", letter(change.kind()), escape(&change.path)))
        .collect()
}

/// Escapes a guest path so it prints on one line: a backslash, a control
/// character or a byte that is not valid UTF-8 becomes `\xNN` (`\u{N}` above
/// U+00FF). The result decodes back to the exact bytes.
fn escape(path: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = path;
    while !rest.is_empty() {
        let (valid, bad) = match std::str::from_utf8(rest) {
            Ok(text) => (text, 0),
            Err(e) => {
                let (text, _) = rest.split_at(e.valid_up_to());
                // Safe split: `valid_up_to` marks the end of valid UTF-8.
                (
                    std::str::from_utf8(text).unwrap_or_default(),
                    e.error_len().unwrap_or(rest.len() - text.len()),
                )
            }
        };
        for ch in valid.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                c if c.is_control() && u32::from(c) <= u32::from(u8::MAX) => {
                    out.push_str(&format!("\\x{:02x}", u32::from(c)));
                }
                c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
                c => out.push(c),
            }
        }
        let skipped = valid.len() + bad;
        for byte in &rest[valid.len()..skipped] {
            out.push_str(&format!("\\x{byte:02x}"));
        }
        rest = &rest[skipped..];
    }
    out
}

/// Docker's API shape: the escaped path and a numeric kind.
fn as_json(change: &FileChange) -> Value {
    let kind = match change.kind() {
        ChangeKind::Added => JSON_KIND_ADDED,
        ChangeKind::Deleted => JSON_KIND_DELETED,
        ChangeKind::Modified | ChangeKind::Unspecified => JSON_KIND_CHANGED,
    };
    json!({"Path": escape(&change.path), "Kind": kind})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(kind: ChangeKind, path: &str) -> FileChange {
        FileChange {
            kind: kind as i32,
            path: path.as_bytes().to_vec(),
        }
    }

    #[test]
    fn given_added_changed_and_deleted_paths_when_rendered_then_each_gets_docker_letter() {
        // Arrange
        let changes = [
            change(ChangeKind::Added, "/added"),
            change(ChangeKind::Modified, "/changed"),
            change(ChangeKind::Deleted, "/deleted"),
        ];

        // Act
        let out = render(&changes);

        // Assert
        assert_eq!(out, "A /added\nC /changed\nD /deleted\n");
    }

    #[test]
    fn given_no_changes_when_rendered_then_nothing_is_printed() {
        // Arrange / Act
        let out = render(&[]);

        // Assert
        assert_eq!(out, "");
    }

    #[test]
    fn given_a_change_when_shown_as_json_then_it_uses_dockers_path_and_kind() {
        // Arrange
        let change = change(ChangeKind::Deleted, "/gone");

        // Act
        let value = as_json(&change);

        // Assert
        assert_eq!(value, json!({"Path": "/gone", "Kind": 2}));
    }

    #[test]
    fn given_a_path_that_forges_a_line_when_rendered_then_control_bytes_are_escaped() {
        // Arrange
        let changes = [change(ChangeKind::Added, "/x\nA /etc/passwd\x1b[2J\\")];

        // Act
        let out = render(&changes);

        // Assert
        assert_eq!(out, "A /x\\x0aA /etc/passwd\\x1b[2J\\\\\n");
    }

    #[test]
    fn given_non_utf8_and_unicode_bytes_when_escaped_then_bytes_are_kept_losslessly() {
        // Arrange
        let path = b"/caf\xc3\xa9/\xff\xfe/\xc2\x85";

        // Act
        let out = escape(path);

        // Assert
        assert_eq!(out, "/caf\u{e9}/\\xff\\xfe/\\x85");
    }

    #[test]
    fn given_a_hostile_path_when_shown_as_json_then_the_path_is_the_escaped_text() {
        // Arrange
        let change = change(ChangeKind::Modified, "/a\nb");

        // Act
        let value = as_json(&change);

        // Assert
        assert_eq!(value, json!({"Path": "/a\\x0ab", "Kind": 0}));
    }
}
