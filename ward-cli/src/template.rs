// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The part of Go's text/template that Docker's `--format` flags use: field
//! paths, pipelines, `if`/`else`/`with`/`range`, variables, parentheses, the
//! functions in `template_funcs.rs`, the `\t` and `\n` escapes, and a `table `
//! prefix that adds a header row and aligns the columns.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::template_funcs::{self as funcs, display, truthy};

const TABLE_PREFIX: &str = "table ";
/// Docker accepts the bare word as shorthand for `{{json .}}`.
const JSON_SHORTHAND: &str = "json";
const JSON_TEMPLATE: &str = "{{json .}}";
const ACTION_OPEN: &str = "{{";
const ACTION_CLOSE: &str = "}}";
const COMMENT_OPEN: &str = "/*";
const COMMENT_CLOSE: &str = "*/";
/// `{{- ` and ` -}}` trim the white space around an action.
const TRIM_MARK: char = '-';
/// Deep enough for any real template, shallow enough for the default stack.
const MAX_NESTING_DEPTH: usize = 200;
const MAX_RANGE_VARIABLES: usize = 2;
/// A column that accepts any field path, for records without a fixed shape.
pub const ANY_FIELD: &str = "*";
const PATH_SEPARATOR: char = '.';
const COLUMN_SEPARATOR: char = '\t';
const COLUMN_PADDING: usize = 3;
/// The column that allows `{{.Label "key"}}`.
pub const LABELS_FIELD: &str = "Labels";
/// The record key that holds the label map `{{.Label "key"}}` reads.
pub const LABEL_MAP_KEY: &str = "Label";
const LABEL_METHOD: &str = "Label";
const ROOT_VARIABLE: &str = "$";

/// One lexed piece of an action.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Word(String),
    /// `.` alone.
    Dot,
    /// `.A.B` on the current value.
    Field(Vec<String>),
    /// `.A` right after a closing parenthesis.
    Chain(Vec<String>),
    Variable(String, Vec<String>),
    Literal(Value),
    Open,
    Close,
    Pipe,
    Comma,
    Declare,
}

#[derive(Debug)]
enum Operand {
    Dot,
    Field(Vec<String>),
    Variable(String, Vec<String>),
    Literal(Value),
    Function(String),
    Group(Box<Pipeline>, Vec<String>),
}

type Command = Vec<Operand>;

#[derive(Debug, Default)]
struct Pipeline {
    variables: Vec<String>,
    commands: Vec<Command>,
}

#[derive(Debug)]
struct Branch {
    pipeline: Pipeline,
    then: Vec<Node>,
    otherwise: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    Text(String),
    Action(Pipeline),
    If(Branch),
    With(Branch),
    Range(Branch),
}

/// How a block ended while parsing.
enum Stop {
    End,
    Else(Vec<Token>),
    Eof,
}

enum Piece {
    Text(String),
    Action(Vec<Token>),
}

/// A parsed `--format` template.
pub struct Template {
    table: bool,
    nodes: Vec<Node>,
    columns: Vec<(String, String)>,
}

impl Template {
    /// Parses `format`. `columns` pairs each field name it may use with the
    /// header title shown above it in a table.
    ///
    /// # Errors
    ///
    /// Fails on a field not in `columns`, a function that does not exist, a
    /// syntax error, or an action that is never closed.
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
        let pieces = split(&body, format)?;
        let mut pieces = pieces.into_iter();
        let (nodes, stop) = parse_block(&mut pieces, 0)?;
        if !matches!(stop, Stop::Eof) {
            bail!("template {format:?} has an unexpected else or end");
        }
        let columns: Vec<(String, String)> = columns
            .iter()
            .map(|(field, title)| ((*field).to_string(), (*title).to_string()))
            .collect();
        let template = Self {
            table,
            nodes,
            columns,
        };
        template.check(&template.nodes, true)?;
        Ok(template)
    }

    fn check(&self, nodes: &[Node], at_root: bool) -> Result<()> {
        for node in nodes {
            match node {
                Node::Text(_) => {}
                Node::Action(pipeline) => self.check_pipeline(pipeline, at_root)?,
                Node::If(branch) => {
                    self.check_pipeline(&branch.pipeline, at_root)?;
                    self.check(&branch.then, at_root)?;
                    self.check(&branch.otherwise, at_root)?;
                }
                Node::With(branch) | Node::Range(branch) => {
                    self.check_pipeline(&branch.pipeline, at_root)?;
                    self.check(&branch.then, false)?;
                    self.check(&branch.otherwise, at_root)?;
                }
            }
        }
        Ok(())
    }

    fn check_pipeline(&self, pipeline: &Pipeline, at_root: bool) -> Result<()> {
        for command in &pipeline.commands {
            for (position, operand) in command.iter().enumerate() {
                match operand {
                    Operand::Field(path) if at_root => self.check_field(path, position == 0)?,
                    Operand::Group(inner, _) => self.check_pipeline(inner, at_root)?,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn check_field(&self, path: &[String], is_head: bool) -> Result<()> {
        let name = path.join(".");
        if is_head && name == LABEL_METHOD {
            if self.columns.iter().any(|(field, _)| field == LABELS_FIELD) {
                return Ok(());
            }
            bail!("unknown field .{name} in --format");
        }
        self.title_of(&name)
            .map(|_| ())
            .ok_or_else(|| anyhow!("unknown field .{name} in --format"))
    }

    /// The table header for the field `name`, if the template may use it.
    fn title_of(&self, name: &str) -> Option<String> {
        let (column, title) = self
            .columns
            .iter()
            .find(|(field, _)| names_field(field, name))?;
        if column == ANY_FIELD {
            let last = name.rsplit(PATH_SEPARATOR).next().unwrap_or(name);
            return Some(last.to_uppercase());
        }
        Some(title.clone())
    }

    /// One line per record, with a header row and aligned columns for a
    /// `table ` template.
    ///
    /// # Errors
    ///
    /// Fails when an action cannot be evaluated, such as `index` out of range.
    pub fn render(&self, records: &[Map<String, Value>]) -> Result<String> {
        let mut lines = Vec::with_capacity(records.len() + 1);
        if self.table {
            lines.push(self.header()?);
        }
        for record in records {
            let root = Value::Object(record.clone());
            let mut run = Run::new(&root);
            run.nodes(&self.nodes, &root)?;
            lines.push(run.out);
        }
        if !self.table {
            return Ok(lines.iter().map(|l| format!("{l}\n")).collect());
        }
        let cells: Vec<Vec<String>> = lines
            .iter()
            .map(|l| l.split(COLUMN_SEPARATOR).map(String::from).collect())
            .collect();
        Ok(align(&cells))
    }

    /// Fields show their title and any other action shows nothing.
    fn header(&self) -> Result<String> {
        let mut out = String::new();
        for node in &self.nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Action(pipeline) => {
                    if let Some(title) = self.pipeline_title(pipeline) {
                        out.push_str(&title);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn pipeline_title(&self, pipeline: &Pipeline) -> Option<String> {
        let first = pipeline.commands.first()?.first()?;
        match first {
            Operand::Field(path) => self.title_of(&path.join(".")),
            _ => None,
        }
    }
}

/// The value at `name`: a key of the record, or a path through nested
/// objects. A key may itself hold dots, such as a label name.
fn lookup<'a>(mut value: &'a Value, path: &[String]) -> Result<&'a Value> {
    let mut rest = path;
    while !rest.is_empty() {
        let Value::Object(map) = value else {
            if value.is_null() {
                return Ok(&Value::Null);
            }
            bail!(
                "can't evaluate field {} in type {}",
                rest[0],
                type_of(value)
            );
        };
        let whole = rest.join(".");
        if let Some(found) = map.get(&whole) {
            return Ok(found);
        }
        let Some(found) = map.get(&rest[0]) else {
            return Ok(&Value::Null);
        };
        value = found;
        rest = &rest[1..];
    }
    Ok(value)
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "bool",
        _ => "value",
    }
}

/// A column ending in `.` stands for every key under it, such as
/// `Actor.Attributes.` for `Actor.Attributes.name`.
fn names_field(column: &str, name: &str) -> bool {
    column == ANY_FIELD || column == name || (column.ends_with('.') && name.starts_with(column))
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

// ---- lexing ----

/// Splits the template into text and lexed actions, applying `-` trims.
fn split(body: &str, format: &str) -> Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut rest = body;
    let mut trim_next = false;
    while let Some((text, tail)) = rest.split_once(ACTION_OPEN) {
        let end = action_end(tail)
            .ok_or_else(|| anyhow!("template {format:?} has an unclosed {ACTION_OPEN}"))?;
        let (action, after) = (&tail[..end], &tail[end + ACTION_CLOSE.len()..]);
        let mut text = if trim_next { text.trim_start() } else { text };
        let mut action = action;
        if let Some(stripped) = trim_marked(action, true) {
            text = text.trim_end();
            action = stripped;
        }
        trim_next = false;
        if let Some(stripped) = trim_marked(action, false) {
            trim_next = true;
            action = stripped;
        }
        pieces.push(Piece::Text(text.to_string()));
        let action = action.trim();
        if !(action.starts_with(COMMENT_OPEN) && action.ends_with(COMMENT_CLOSE)) {
            pieces.push(Piece::Action(lex(action)?));
        }
        rest = after;
    }
    let rest = if trim_next { rest.trim_start() } else { rest };
    pieces.push(Piece::Text(rest.to_string()));
    Ok(pieces)
}

fn char_literal(chars: &[char], start: usize) -> Result<(char, usize)> {
    let (character, end) = match (chars.get(start + 1), chars.get(start + 2)) {
        (Some('\\'), Some(escaped)) => (
            match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => *other,
            },
            start + 3,
        ),
        (Some(&c), _) if c != '\'' => (c, start + 2),
        _ => bail!("malformed character constant"),
    };
    if chars.get(end) != Some(&'\'') {
        bail!("unterminated character constant");
    }
    Ok((character, end + 1))
}

/// Where the closing `}}` of the action in `tail` starts. A `}}` inside a
/// quoted string or a comment does not close it.
fn action_end(tail: &str) -> Option<usize> {
    let body = tail.trim_start_matches(TRIM_MARK).trim_start();
    let mut from = tail.len() - body.len();
    if body.starts_with(COMMENT_OPEN) {
        from += body.find(COMMENT_CLOSE)? + COMMENT_CLOSE.len();
        return tail[from..].find(ACTION_CLOSE).map(|at| from + at);
    }
    let mut quote: Option<char> = None;
    let mut chars = tail[from..].char_indices();
    while let Some((offset, c)) = chars.next() {
        match (quote, c) {
            (Some('"' | '\''), '\\') => {
                chars.next();
            }
            (Some(open), _) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '`' | '\'') => quote = Some(c),
            (None, _) if tail[from + offset..].starts_with(ACTION_CLOSE) => {
                return Some(from + offset);
            }
            (None, _) => {}
        }
    }
    None
}

/// The action without its `-` trim mark, if it has one on the given side.
fn trim_marked(action: &str, leading: bool) -> Option<&str> {
    if leading {
        let rest = action.strip_prefix(TRIM_MARK)?;
        rest.starts_with(char::is_whitespace).then_some(rest)
    } else {
        let rest = action.strip_suffix(TRIM_MARK)?;
        rest.ends_with(char::is_whitespace).then_some(rest)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn lex(action: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = action.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            _ if c.is_whitespace() => i += 1,
            '(' | ')' | '|' | ',' => {
                tokens.push(match c {
                    '(' => Token::Open,
                    ')' => Token::Close,
                    '|' => Token::Pipe,
                    _ => Token::Comma,
                });
                i += 1;
            }
            ':' if chars.get(i + 1) == Some(&'=') => {
                tokens.push(Token::Declare);
                i += 2;
            }
            '"' => {
                let (text, next) = quoted(&chars, i)?;
                tokens.push(Token::Literal(Value::String(text)));
                i = next;
            }
            '`' => {
                let Some(length) = chars[i + 1..].iter().position(|&c| c == '`') else {
                    bail!("unterminated raw quoted string");
                };
                let text: String = chars[i + 1..i + 1 + length].iter().collect();
                tokens.push(Token::Literal(Value::String(text)));
                i += length + 2;
            }
            '\'' => {
                let (character, next) = char_literal(&chars, i)?;
                tokens.push(Token::Literal(Value::from(u32::from(character))));
                i = next;
            }
            '$' => {
                let (name, next) = word(&chars, i + 1);
                let (path, next) = field_path(&chars, next);
                tokens.push(Token::Variable(format!("${name}"), path));
                i = next;
            }
            '.' => {
                let (path, next) = field_path(&chars, i);
                if path.is_empty() {
                    tokens.push(Token::Dot);
                    i += 1;
                } else if matches!(tokens.last(), Some(Token::Close))
                    && i > 0
                    && chars[i - 1] == ')'
                {
                    tokens.push(Token::Chain(path));
                    i = next;
                } else {
                    tokens.push(Token::Field(path));
                    i = next;
                }
            }
            _ if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let end = chars[i + 1..]
                    .iter()
                    .position(|c| !(c.is_ascii_digit() || *c == '.'))
                    .map_or(chars.len(), |p| i + 1 + p);
                let text: String = chars[i..end].iter().collect();
                tokens.push(Token::Literal(parse_number(&text)?));
                i = end;
            }
            _ if is_word_char(c) => {
                let (name, next) = word(&chars, i);
                tokens.push(match name.as_str() {
                    "true" => Token::Literal(Value::Bool(true)),
                    "false" => Token::Literal(Value::Bool(false)),
                    "nil" => Token::Literal(Value::Null),
                    _ => Token::Word(name),
                });
                i = next;
            }
            other => bail!("unexpected {other:?} in {{{{{action}}}}}"),
        }
    }
    Ok(tokens)
}

fn parse_number(text: &str) -> Result<Value> {
    if let Ok(int) = text.parse::<i64>() {
        return Ok(Value::from(int));
    }
    text.parse::<f64>()
        .ok()
        .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number))
        .ok_or_else(|| anyhow!("bad number syntax: {text:?}"))
}

fn word(chars: &[char], start: usize) -> (String, usize) {
    let end = chars[start..]
        .iter()
        .position(|&c| !is_word_char(c))
        .map_or(chars.len(), |p| start + p);
    (chars[start..end].iter().collect(), end)
}

/// `.A.B.C` starting at a dot. Empty when the dot stands alone.
fn field_path(chars: &[char], start: usize) -> (Vec<String>, usize) {
    let mut path = Vec::new();
    let mut i = start;
    while chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|&c| is_word_char(c)) {
        let (name, next) = word(chars, i + 1);
        path.push(name);
        i = next;
    }
    (path, i)
}

fn quoted(chars: &[char], start: usize) -> Result<(String, usize)> {
    let mut text = String::new();
    let mut i = start + 1;
    while let Some(&c) = chars.get(i) {
        match c {
            '"' => return Ok((text, i + 1)),
            '\\' => {
                i += 1;
                text.push(match chars.get(i) {
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('r') => '\r',
                    Some(&other) => other,
                    None => break,
                });
            }
            other => text.push(other),
        }
        i += 1;
    }
    bail!("unterminated quoted string")
}

// ---- parsing ----

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAX_NESTING_DEPTH {
        bail!("template nests deeper than {MAX_NESTING_DEPTH} levels");
    }
    Ok(())
}

fn parse_block(pieces: &mut std::vec::IntoIter<Piece>, depth: usize) -> Result<(Vec<Node>, Stop)> {
    check_depth(depth)?;
    let mut nodes = Vec::new();
    while let Some(piece) = pieces.next() {
        let tokens = match piece {
            Piece::Text(text) => {
                if !text.is_empty() {
                    nodes.push(Node::Text(text));
                }
                continue;
            }
            Piece::Action(tokens) => tokens,
        };
        match tokens.first() {
            Some(Token::Word(word)) if word == "end" => return Ok((nodes, Stop::End)),
            Some(Token::Word(word)) if word == "else" => {
                return Ok((nodes, Stop::Else(tokens[1..].to_vec())));
            }
            Some(Token::Word(word)) if matches!(word.as_str(), "if" | "with" | "range") => {
                let word = word.clone();
                nodes.push(parse_branch(&word, &tokens[1..], pieces, depth + 1)?);
            }
            Some(Token::Word(word))
                if matches!(
                    word.as_str(),
                    "define" | "template" | "block" | "break" | "continue"
                ) =>
            {
                bail!("unsupported action {word:?} in --format")
            }
            Some(_) => nodes.push(Node::Action(parse_pipeline_tokens(&tokens, depth)?)),
            None => bail!("missing value for command"),
        }
    }
    Ok((nodes, Stop::Eof))
}

fn parse_branch(
    keyword: &str,
    tokens: &[Token],
    pieces: &mut std::vec::IntoIter<Piece>,
    depth: usize,
) -> Result<Node> {
    check_depth(depth)?;
    let pipeline = parse_pipeline_tokens(tokens, depth)?;
    if keyword == "range" && pipeline.variables.len() > MAX_RANGE_VARIABLES {
        bail!("range can only initialize {MAX_RANGE_VARIABLES} variables");
    }
    let (then, stop) = parse_block(pieces, depth)?;
    let otherwise = match stop {
        Stop::End => Vec::new(),
        Stop::Eof => bail!("unexpected EOF: missing {{{{end}}}} for {keyword}"),
        Stop::Else(rest) => match rest.split_first() {
            Some((Token::Word(word), tail)) if word == keyword && keyword != "range" => {
                vec![parse_branch(keyword, tail, pieces, depth + 1)?]
            }
            Some(_) => bail!("unexpected tokens after else"),
            None => {
                let (otherwise, stop) = parse_block(pieces, depth)?;
                match stop {
                    Stop::End => otherwise,
                    Stop::Else(_) => bail!("unexpected {{{{else}}}} in {keyword}"),
                    Stop::Eof => bail!("unexpected EOF: missing {{{{end}}}} for {keyword}"),
                }
            }
        },
    };
    let branch = Branch {
        pipeline,
        then,
        otherwise,
    };
    Ok(match keyword {
        "if" => Node::If(branch),
        "with" => Node::With(branch),
        _ => Node::Range(branch),
    })
}

fn parse_pipeline_tokens(tokens: &[Token], depth: usize) -> Result<Pipeline> {
    let mut position = 0;
    let pipeline = parse_pipeline(tokens, &mut position, depth)?;
    if position < tokens.len() {
        bail!("unexpected {:?} in --format", tokens[position]);
    }
    Ok(pipeline)
}

fn parse_pipeline(tokens: &[Token], position: &mut usize, depth: usize) -> Result<Pipeline> {
    check_depth(depth)?;
    let mut pipeline = Pipeline::default();
    let names = declaration(&tokens[*position..]);
    if let Some((names, consumed)) = names {
        pipeline.variables = names;
        *position += consumed;
    }
    loop {
        let mut command = Vec::new();
        while let Some(token) = tokens.get(*position) {
            match token {
                Token::Pipe | Token::Close => break,
                _ => command.push(parse_operand(tokens, position, depth)?),
            }
        }
        if command.is_empty() {
            bail!("missing value for command");
        }
        if let Some(Operand::Literal(_)) = command.first().filter(|_| command.len() > 1) {
            bail!("non-executable command in pipeline");
        }
        pipeline.commands.push(command);
        if tokens.get(*position) == Some(&Token::Pipe) {
            *position += 1;
        } else {
            return Ok(pipeline);
        }
    }
}

/// `$a :=` or `$a, $b :=` at the start of a pipeline.
fn declaration(tokens: &[Token]) -> Option<(Vec<String>, usize)> {
    let mut names = Vec::new();
    let mut i = 0;
    loop {
        let Some(Token::Variable(name, path)) = tokens.get(i) else {
            return None;
        };
        if !path.is_empty() {
            return None;
        }
        names.push(name.clone());
        i += 1;
        match tokens.get(i) {
            Some(Token::Comma) => i += 1,
            Some(Token::Declare) => return Some((names, i + 1)),
            _ => return None,
        }
    }
}

fn parse_operand(tokens: &[Token], position: &mut usize, depth: usize) -> Result<Operand> {
    let token = tokens[*position].clone();
    *position += 1;
    Ok(match token {
        Token::Dot => Operand::Dot,
        Token::Field(path) | Token::Chain(path) => Operand::Field(path),
        Token::Variable(name, path) => Operand::Variable(name, path),
        Token::Literal(value) => Operand::Literal(value),
        Token::Word(name) => {
            if !funcs::is_function(&name) {
                bail!("function {name:?} not defined");
            }
            Operand::Function(name)
        }
        Token::Open => {
            let inner = parse_pipeline(tokens, position, depth + 1)?;
            if tokens.get(*position) != Some(&Token::Close) {
                bail!("unclosed left paren");
            }
            *position += 1;
            let path = match tokens.get(*position) {
                Some(Token::Chain(path)) => {
                    *position += 1;
                    path.clone()
                }
                _ => Vec::new(),
            };
            Operand::Group(Box::new(inner), path)
        }
        other => bail!("unexpected {other:?} in --format"),
    })
}

// ---- evaluation ----

struct Run {
    root: Value,
    variables: Vec<(String, Value)>,
    out: String,
}

impl Run {
    fn new(root: &Value) -> Self {
        Self {
            root: root.clone(),
            variables: Vec::new(),
            out: String::new(),
        }
    }

    fn nodes(&mut self, nodes: &[Node], dot: &Value) -> Result<()> {
        let scope = self.variables.len();
        for node in nodes {
            match node {
                Node::Text(text) => self.out.push_str(text),
                Node::Action(pipeline) => {
                    let value = self.pipeline(pipeline, dot)?;
                    if pipeline.variables.is_empty() {
                        self.out.push_str(&display(&value));
                    }
                }
                Node::If(branch) => {
                    // Variables of the condition live until the block ends.
                    let block = self.variables.len();
                    let value = self.pipeline(&branch.pipeline, dot)?;
                    let chosen = if truthy(&value) {
                        &branch.then
                    } else {
                        &branch.otherwise
                    };
                    self.nodes(chosen, dot)?;
                    self.variables.truncate(block);
                }
                Node::With(branch) => {
                    let block = self.variables.len();
                    let value = self.pipeline(&branch.pipeline, dot)?;
                    if truthy(&value) {
                        self.nodes(&branch.then, &value)?;
                    } else {
                        self.nodes(&branch.otherwise, dot)?;
                    }
                    self.variables.truncate(block);
                }
                Node::Range(branch) => self.range(branch, dot)?,
            }
        }
        self.variables.truncate(scope);
        Ok(())
    }

    fn range(&mut self, branch: &Branch, dot: &Value) -> Result<()> {
        let value = self.pipeline_value(&branch.pipeline, dot)?;
        let items: Vec<(Value, Value)> = match &value {
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| (Value::from(i), item.clone()))
                .collect(),
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                keys.into_iter()
                    .map(|k| (Value::String(k.clone()), map[k].clone()))
                    .collect()
            }
            Value::Null => Vec::new(),
            other => bail!("range can't iterate over {}", display(other)),
        };
        if items.is_empty() {
            return self.nodes(&branch.otherwise, dot);
        }
        let names = &branch.pipeline.variables;
        for (key, item) in items {
            let scope = self.variables.len();
            match names.as_slice() {
                [] => {}
                [element] => self.variables.push((element.clone(), item.clone())),
                [index, element, ..] => {
                    self.variables.push((index.clone(), key));
                    self.variables.push((element.clone(), item.clone()));
                }
            }
            self.nodes(&branch.then, &item)?;
            self.variables.truncate(scope);
        }
        Ok(())
    }

    /// The value of a pipeline's commands, each fed the one before.
    fn pipeline_value(&mut self, pipeline: &Pipeline, dot: &Value) -> Result<Value> {
        let mut piped: Option<Value> = None;
        for command in &pipeline.commands {
            piped = Some(self.command(command, dot, piped.take())?);
        }
        Ok(piped.unwrap_or(Value::Null))
    }

    /// Runs the pipeline and binds its variables in the current scope.
    fn pipeline(&mut self, pipeline: &Pipeline, dot: &Value) -> Result<Value> {
        let value = self.pipeline_value(pipeline, dot)?;
        for name in &pipeline.variables {
            self.variables.push((name.clone(), value.clone()));
        }
        Ok(value)
    }

    fn command(&mut self, command: &Command, dot: &Value, piped: Option<Value>) -> Result<Value> {
        let (head, rest) = command
            .split_first()
            .ok_or_else(|| anyhow!("missing value for command"))?;
        let mut args = Vec::with_capacity(rest.len() + 1);
        // `and` stops at the first false operand and `or` at the first true
        // one, so the operands after it are never evaluated.
        let stop_on = match head {
            Operand::Function(name) if name == "and" => Some(false),
            Operand::Function(name) if name == "or" => Some(true),
            _ => None,
        };
        for operand in rest {
            let value = self.operand(operand, dot)?;
            if stop_on == Some(truthy(&value)) {
                return Ok(value);
            }
            args.push(value);
        }
        args.extend(piped);
        match head {
            Operand::Function(name) => funcs::call(name, &args),
            Operand::Field(path)
                if path.len() == 1 && path[0] == LABEL_METHOD && !args.is_empty() =>
            {
                label(dot, &args)
            }
            _ if !args.is_empty() => bail!("can't give argument to non-function"),
            other => self.operand(other, dot),
        }
    }

    fn operand(&mut self, operand: &Operand, dot: &Value) -> Result<Value> {
        match operand {
            Operand::Dot => Ok(dot.clone()),
            Operand::Field(path) => Ok(lookup(dot, path)?.clone()),
            Operand::Literal(value) => Ok(value.clone()),
            Operand::Variable(name, path) => {
                let base = if name == ROOT_VARIABLE {
                    &self.root
                } else {
                    &self
                        .variables
                        .iter()
                        .rev()
                        .find(|(n, _)| n == name)
                        .ok_or_else(|| anyhow!("undefined variable: {name}"))?
                        .1
                };
                Ok(lookup(base, path)?.clone())
            }
            Operand::Function(name) => funcs::call(name, &[]),
            Operand::Group(inner, path) => {
                let value = self.pipeline_value(inner, dot)?;
                Ok(lookup(&value, path)?.clone())
            }
        }
    }
}

/// `{{.Label "key"}}`: one entry of the record's label map.
fn label(dot: &Value, args: &[Value]) -> Result<Value> {
    let [Value::String(key)] = args else {
        bail!("Label takes one string argument");
    };
    let found = dot
        .get(LABEL_MAP_KEY)
        .and_then(|labels| labels.get(key))
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COLUMNS: [(&str, &str); 2] = [("Name", "NAME"), ("PIDs", "PIDS")];
    const ANY: [(&str, &str); 1] = [(ANY_FIELD, "")];

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    fn records() -> Vec<Map<String, Value>> {
        vec![
            object(json!({"Name": "web1", "PIDs": 3})),
            object(json!({"Name": "db", "PIDs": 12})),
        ]
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

        assert_eq!(template.render(&records()).expect("render"), expected);
    }

    #[test]
    fn given_label_action_when_rendered_then_the_label_value_or_empty() {
        // Arrange
        let record = object(json!({"Labels": "a=1", "Label": {"a": "1"}}));
        let columns = [(LABELS_FIELD, "LABELS")];
        let template =
            Template::parse("{{.Label \"a\"}}|{{.Label \"b\"}}", &columns).expect("parse");

        // Act
        let out = template.render(&[record]).expect("render");

        // Assert
        assert_eq!(out, "1|\n");
    }

    #[test]
    fn given_label_action_without_a_labels_column_when_parsed_then_it_is_refused() {
        // Act
        let result = Template::parse("{{.Label \"a\"}}", &COLUMNS);

        // Assert
        assert!(result.is_err());
    }

    #[rstest::rstest]
    #[case::unknown_field("{{.Nope}}", ".Nope")]
    #[case::unknown_function("{{frobnicate .Name}}", "frobnicate")]
    #[case::unclosed_action("{{.Name", "unclosed")]
    #[case::missing_end("{{if .Name}}x", "end")]
    #[case::stray_end("{{end}}", "end")]
    #[case::unknown_field_in_if("{{if .Nope}}x{{end}}", ".Nope")]
    fn given_a_bad_template_when_parsed_then_the_error_names_the_problem(
        #[case] format: &str,
        #[case] named: &str,
    ) {
        let error = Template::parse(format, &COLUMNS).err().expect("rejected");

        assert!(error.to_string().contains(named), "{error}");
    }

    fn deep() -> Map<String, Value> {
        object(json!({
            "State": {"Status": "running", "Code": 3},
            "Tags": ["a", "b"],
            "Env": {"B": "2", "A": "1"},
            "Name": "web",
            "Empty": [],
        }))
    }

    #[rstest::rstest]
    #[case::nested("{{.State.Status}}", "running")]
    #[case::json_of_path("{{json .State}}", "{\"Code\":3,\"Status\":\"running\"}")]
    #[case::missing_is_empty("[{{.State.Nope}}]", "[]")]
    #[case::array_prints_like_go("{{.Tags}}", "[a b]")]
    #[case::map_prints_sorted("{{.Env}}", "map[A:1 B:2]")]
    #[case::if_true("{{if .Name}}yes{{end}}", "yes")]
    #[case::if_else("{{if .Empty}}yes{{else}}no{{end}}", "no")]
    #[case::else_if("{{if .Empty}}a{{else if eq .Name \"web\"}}b{{else}}c{{end}}", "b")]
    #[case::eq_pipe("{{if eq .State.Status \"running\" \"exited\"}}up{{end}}", "up")]
    #[case::and_or_not(
        "{{if and .Name (not .Empty)}}x{{end}}{{if or .Empty .Name}}y{{end}}",
        "xy"
    )]
    #[case::range_array("{{range .Tags}}<{{.}}>{{end}}", "<a><b>")]
    #[case::range_index("{{range $i, $t := .Tags}}{{$i}}={{$t}} {{end}}", "0=a 1=b ")]
    #[case::range_map("{{range $k, $v := .Env}}{{$k}}={{$v}};{{end}}", "A=1;B=2;")]
    #[case::range_else("{{range .Empty}}x{{else}}none{{end}}", "none")]
    #[case::range_root("{{range .Tags}}{{$.Name}}{{.}} {{end}}", "weba webb ")]
    #[case::with("{{with .State}}{{.Status}}{{end}}", "running")]
    #[case::variable("{{$n := .Name}}{{$n}}{{$n}}", "webweb")]
    #[case::index_array("{{index .Tags 1}}", "b")]
    #[case::index_map("{{index .Env \"A\"}}", "1")]
    #[case::group_field("{{(index .Tags 0)}}", "a")]
    #[case::len("{{len .Tags}}", "2")]
    #[case::join("{{join .Tags \", \"}}", "a, b")]
    #[case::split_index("{{index (split \"x:y\" \":\") 1}}", "y")]
    #[case::pipe_upper("{{.Name | upper}}", "WEB")]
    #[case::pipe_chain("{{.Name | upper | printf \"%s!\"}}", "WEB!")]
    #[case::printf("{{printf \"%-5s|%03d\" .Name .State.Code}}", "web  |003")]
    #[case::title("{{title .Name}}", "Web")]
    #[case::truncate("{{truncate .Name 2}}", "we")]
    #[case::slice("{{slice .Name 1 3}}", "eb")]
    #[case::println("{{println .Name .State.Code}}", "web 3\n")]
    #[case::lower("{{lower \"AB\"}}", "ab")]
    #[case::trim_markers("a {{- \"b\" -}} c", "abc")]
    #[case::comment("a{{/* hidden */}}b", "ab")]
    #[case::number_compare("{{if gt .State.Code 2}}big{{end}}", "big")]
    #[case::declare_group("{{$x := (index .Tags 0)}}{{$x}}", "a")]
    #[case::declare_in_if("{{if $n := (len .Tags)}}{{$n}}{{end}}", "2")]
    #[case::range_group("{{range $i, $v := (split \"x:y\" \":\")}}{{$i}}{{end}}", "01")]
    #[case::quoted_close("{{\"}}\"}}", "}}")]
    #[case::comment_close("a{{/* x }} y */}}b", "ab")]
    #[case::else_with("{{with .Empty}}x{{else with .Name}}{{.}}{{end}}", "web")]
    #[case::and_short_circuits("{{if and .Empty (index .Empty 0)}}x{{end}}", "")]
    #[case::or_short_circuits("{{if or .Name (index .Empty 0)}}y{{end}}", "y")]
    fn given_go_syntax_when_rendered_then_it_matches_go_templates(
        #[case] format: &str,
        #[case] expected: &str,
    ) {
        let template = Template::parse(format, &ANY).expect("parse");

        let out = template.render(&[deep()]).expect("render");

        assert_eq!(out, format!("{expected}\n"));
    }

    #[test]
    fn given_a_variable_of_an_if_condition_when_used_after_end_then_it_is_undefined() {
        let template = Template::parse("{{if $n := .Name}}{{end}}{{$n}}", &ANY).expect("parse");

        let error = template.render(&[deep()]).expect_err("fails");

        assert!(error.to_string().contains("undefined variable"), "{error}");
    }

    #[test]
    fn given_an_index_out_of_range_when_rendered_then_the_error_is_returned() {
        let template = Template::parse("{{index .Tags 9}}", &ANY).expect("parse");

        let error = template.render(&[deep()]).expect_err("fails");

        assert!(error.to_string().contains("out of range"), "{error}");
    }

    #[test]
    fn given_a_table_with_nested_fields_when_rendered_then_headers_use_the_last_name() {
        let template = Template::parse("table {{.Name}}\\t{{.State.Status}}", &ANY).expect("parse");

        let out = template.render(&[deep()]).expect("render");

        assert_eq!(out, "NAME   STATUS\nweb    running\n");
    }

    #[test]
    fn given_deeply_nested_blocks_when_parsed_then_it_fails_without_overflowing() {
        let depth = 20_000;
        let source = format!("{}x{}", "{{if 1}}".repeat(depth), "{{end}}".repeat(depth));

        let error = Template::parse(&source, &ANY).err().expect("fails");

        assert!(error.to_string().contains("nests deeper"), "{error}");
    }

    #[test]
    fn given_deeply_nested_parentheses_when_parsed_then_it_fails_without_overflowing() {
        let depth = 20_000;
        let source = format!("{{{{{}1{}}}}}", "(".repeat(depth), ")".repeat(depth));

        let error = Template::parse(&source, &ANY).err().expect("fails");

        assert!(error.to_string().contains("nests deeper"), "{error}");
    }

    #[test]
    fn given_a_long_else_if_chain_when_parsed_then_it_fails_without_overflowing() {
        let source = format!("{{{{if 0}}}}{}{{{{end}}}}", "{{else if 0}}".repeat(20_000));

        let error = Template::parse(&source, &ANY).err().expect("fails");

        assert!(error.to_string().contains("nests deeper"), "{error}");
    }

    #[test]
    fn given_two_else_actions_when_parsed_then_it_fails() {
        let error = Template::parse("{{if 1}}a{{else}}b{{else}}c{{end}}", &ANY)
            .err()
            .expect("fails");

        assert!(error.to_string().contains("unexpected {{else}}"), "{error}");
    }

    #[test]
    fn given_a_range_with_three_variables_when_parsed_then_it_fails() {
        let error = Template::parse("{{range $a, $b, $c := .Tags}}{{end}}", &ANY)
            .err()
            .expect("fails");

        assert!(error.to_string().contains("range can only"), "{error}");
    }

    #[rstest::rstest]
    #[case::raw_string("{{`a}}b`}}", "a}}b")]
    #[case::char_literal("{{'a'}}", "97")]
    #[case::escaped_char_literal("{{'\\n'}}", "10")]
    fn given_go_literals_when_rendered_then_they_match_go(
        #[case] format: &str,
        #[case] expected: &str,
    ) {
        let template = Template::parse(format, &ANY).expect("parse");

        let out = template.render(&[deep()]).expect("render");

        assert_eq!(out, format!("{expected}\n"));
    }
}
