// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The functions of Go's text/template and Docker's `--format` that
//! `template.rs` can call, and how a value prints.

use std::cmp::Ordering;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// Names callable from a template.
const FUNCTIONS: [&str; 22] = [
    "and", "or", "not", "eq", "ne", "lt", "le", "gt", "ge", "index", "len", "slice", "join",
    "split", "lower", "upper", "title", "truncate", "json", "print", "println", "printf",
];
const DECIMAL_RADIX: u32 = 10;
const DEFAULT_FLOAT_PRECISION: usize = 6;

pub fn is_function(name: &str) -> bool {
    FUNCTIONS.contains(&name)
}

/// Go's notion of a true value: not zero, empty or nil.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// How Go prints a value with `%v`: arrays as `[a b]`, maps as `map[k:v]`
/// with sorted keys. A missing value prints as nothing at the top level.
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => nested(other),
    }
}

fn nested(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(nested).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{key}:{}", nested(&map[key])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
        other => other.to_string(),
    }
}

/// Calls the function `name`; the parser has already checked that it exists.
pub fn call(name: &str, args: &[Value]) -> Result<Value> {
    match name {
        "and" => fold_logic(args, false),
        "or" => fold_logic(args, true),
        "not" => Ok(Value::Bool(!truthy(exactly(name, args, 1)?[0]))),
        "eq" => equals(args),
        "ne" => Ok(Value::Bool(!equals(args)?.as_bool().unwrap_or(false))),
        "lt" => ordered(name, args, |o| o == Ordering::Less),
        "le" => ordered(name, args, |o| o != Ordering::Greater),
        "gt" => ordered(name, args, |o| o == Ordering::Greater),
        "ge" => ordered(name, args, |o| o != Ordering::Less),
        "index" => index(args),
        "len" => length(exactly(name, args, 1)?[0]),
        "slice" => slice(args),
        "join" => join(args),
        "split" => split(args),
        "lower" => Ok(Value::String(text(name, args)?.to_lowercase())),
        "upper" => Ok(Value::String(text(name, args)?.to_uppercase())),
        "title" => Ok(Value::String(title(&text(name, args)?))),
        "truncate" => truncate(args),
        "json" => Ok(Value::String(exactly(name, args, 1)?[0].to_string())),
        "print" => Ok(Value::String(print(args))),
        "println" => Ok(Value::String(spaced(args) + "\n")),
        "printf" => printf(args),
        _ => bail!("function {name:?} not defined"),
    }
}

fn exactly<'a>(name: &str, args: &'a [Value], count: usize) -> Result<Vec<&'a Value>> {
    if args.len() != count {
        bail!(
            "wrong number of args for {name}: want {count} got {}",
            args.len()
        );
    }
    Ok(args.iter().collect())
}

fn text(name: &str, args: &[Value]) -> Result<String> {
    Ok(display(exactly(name, args, 1)?[0]))
}

/// `and` returns its first false argument, else the last; `or` the first
/// true one, else the last.
fn fold_logic(args: &[Value], stop_on: bool) -> Result<Value> {
    let Some(last) = args.last() else {
        bail!("wrong number of args: want at least 1 got 0");
    };
    Ok(args
        .iter()
        .find(|v| truthy(v) == stop_on)
        .unwrap_or(last)
        .clone())
}

fn compare(a: &Value, b: &Value) -> Result<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => Ok(x.cmp(&y)),
            _ => x
                .as_f64()
                .partial_cmp(&y.as_f64())
                .ok_or_else(|| anyhow!("cannot compare numbers")),
        },
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        _ => bail!("incompatible types for comparison"),
    }
}

fn equals(args: &[Value]) -> Result<Value> {
    let Some((first, rest)) = args.split_first().filter(|(_, rest)| !rest.is_empty()) else {
        bail!("missing argument for comparison");
    };
    let mut matched = false;
    for other in rest {
        matched |= compare(first, other)? == Ordering::Equal;
    }
    Ok(Value::Bool(matched))
}

fn ordered(name: &str, args: &[Value], accept: fn(Ordering) -> bool) -> Result<Value> {
    let pair = exactly(name, args, 2)?;
    Ok(Value::Bool(accept(compare(pair[0], pair[1])?)))
}

fn index(args: &[Value]) -> Result<Value> {
    let Some((first, keys)) = args.split_first() else {
        bail!("wrong number of args for index: want at least 1 got 0");
    };
    let mut current = first.clone();
    for key in keys {
        current = match (&current, key) {
            (Value::Array(items), Value::Number(n)) => {
                let at = n.as_i64().ok_or_else(|| anyhow!("cannot index with {n}"))?;
                usize::try_from(at)
                    .ok()
                    .and_then(|i| items.get(i))
                    .cloned()
                    .ok_or_else(|| anyhow!("error calling index: index out of range: {at}"))?
            }
            (Value::Object(map), Value::String(name)) => {
                map.get(name).cloned().unwrap_or(Value::Null)
            }
            (other, _) => bail!(
                "error calling index: can't index item of type {}",
                kind(other)
            ),
        };
    }
    Ok(current)
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "nil",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "map",
    }
}

fn count_of(value: &Value) -> Result<usize> {
    match value {
        Value::String(s) => Ok(s.len()),
        Value::Array(a) => Ok(a.len()),
        Value::Object(o) => Ok(o.len()),
        other => bail!("error calling len: len of type {}", kind(other)),
    }
}

fn length(value: &Value) -> Result<Value> {
    Ok(Value::from(count_of(value)?))
}

fn position(value: &Value) -> Result<usize> {
    value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| anyhow!("bad index {value}"))
}

fn slice(args: &[Value]) -> Result<Value> {
    let Some((target, bounds)) = args.split_first().filter(|(_, b)| b.len() <= 2) else {
        bail!("wrong number of args for slice");
    };
    let len = count_of(target)?;
    let from = bounds.first().map(position).transpose()?.unwrap_or(0);
    let to = bounds.get(1).map(position).transpose()?.unwrap_or(len);
    if from > to || to > len {
        bail!("error calling slice: index out of range: {to}");
    }
    match target {
        Value::String(s) => s
            .get(from..to)
            .map(|part| Value::String(part.to_string()))
            .ok_or_else(|| anyhow!("error calling slice: index is not a character boundary")),
        Value::Array(items) => Ok(Value::Array(items[from..to].to_vec())),
        other => bail!(
            "error calling slice: can't slice item of type {}",
            kind(other)
        ),
    }
}

fn join(args: &[Value]) -> Result<Value> {
    let pair = exactly("join", args, 2)?;
    let Value::Array(items) = pair[0] else {
        bail!("error calling join: want an array, got {}", kind(pair[0]));
    };
    let parts: Vec<String> = items.iter().map(display).collect();
    Ok(Value::String(parts.join(&display(pair[1]))))
}

fn split(args: &[Value]) -> Result<Value> {
    let pair = exactly("split", args, 2)?;
    let (text, separator) = (display(pair[0]), display(pair[1]));
    let parts: Vec<Value> = if separator.is_empty() {
        text.chars().map(|c| Value::String(c.to_string())).collect()
    } else {
        text.split(separator.as_str())
            .map(|p| Value::String(p.to_string()))
            .collect()
    };
    Ok(Value::Array(parts))
}

/// Go's `strings.Title`: a letter after a non-letter, digit or underscore
/// becomes upper case.
fn title(text: &str) -> String {
    let mut previous_in_word = false;
    text.chars()
        .map(|c| {
            let shown = if previous_in_word {
                c.to_string()
            } else {
                c.to_uppercase().to_string()
            };
            previous_in_word = c.is_alphanumeric() || c == '_';
            shown
        })
        .collect()
}

fn truncate(args: &[Value]) -> Result<Value> {
    let pair = exactly("truncate", args, 2)?;
    let keep = position(pair[1])?;
    Ok(Value::String(display(pair[0]).chars().take(keep).collect()))
}

/// Go's `fmt.Sprint`: a space between two operands when neither is a string.
fn print(args: &[Value]) -> String {
    let is_text = |v: &Value| matches!(v, Value::String(_));
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 && !is_text(arg) && !is_text(&args[i - 1]) {
            out.push(' ');
        }
        out.push_str(&display(arg));
    }
    out
}

/// Go's `fmt.Sprintln` without the newline: a space between every operand.
fn spaced(args: &[Value]) -> String {
    let parts: Vec<String> = args.iter().map(display).collect();
    parts.join(" ")
}

/// Flags, width and precision of one `printf` verb.
#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn number(chars: &mut Chars) -> usize {
    let mut total = 0;
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
        total = total * 10 + digit.to_digit(DECIMAL_RADIX).unwrap_or(0) as usize;
    }
    total
}

fn spec(chars: &mut Chars) -> Spec {
    let mut spec = Spec::default();
    while let Some(flag) = chars.next_if(|f| matches!(f, '-' | '0' | '+' | ' ' | '#')) {
        match flag {
            '-' => spec.left = true,
            '0' => spec.zero = true,
            _ => {}
        }
    }
    spec.width = number(chars);
    if chars.next_if_eq(&'.').is_some() {
        spec.precision = Some(number(chars));
    }
    spec
}

fn printf(args: &[Value]) -> Result<Value> {
    let Some((Value::String(format), rest)) = args.split_first() else {
        bail!("printf needs a format string");
    };
    let mut values = rest.iter();
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = spec(&mut chars);
        let Some(verb) = chars.next() else {
            out.push_str("%!(NOVERB)");
            break;
        };
        if verb == '%' {
            out.push('%');
            continue;
        }
        match values.next() {
            Some(value) => out.push_str(&pad(&verb_text(verb, value, &spec), &spec)),
            None => out.push_str(&format!("%!{verb}(MISSING)")),
        }
    }
    Ok(Value::String(out))
}

fn verb_text(verb: char, value: &Value, spec: &Spec) -> String {
    match verb {
        's' | 'v' => {
            let shown = display(value);
            match spec.precision {
                Some(keep) => shown.chars().take(keep).collect(),
                None => shown,
            }
        }
        'q' => Value::String(display(value)).to_string(),
        'd' | 't' => display(value),
        'f' => match value.as_f64() {
            Some(f) => format!("{f:.*}", spec.precision.unwrap_or(DEFAULT_FLOAT_PRECISION)),
            None => format!("%!f({}={})", kind(value), display(value)),
        },
        'x' => match value {
            Value::Number(n) => format!("{:x}", n.as_i64().unwrap_or_default()),
            other => display(other).bytes().map(|b| format!("{b:02x}")).collect(),
        },
        other => format!("%!{other}({}={})", kind(value), display(value)),
    }
}

fn pad(text: &str, spec: &Spec) -> String {
    let missing = spec.width.saturating_sub(text.chars().count());
    if spec.left {
        format!("{text}{}", " ".repeat(missing))
    } else if spec.zero {
        let (sign, digits) = text
            .strip_prefix('-')
            .map_or(("", text), |digits| ("-", digits));
        format!("{sign}{}{digits}", "0".repeat(missing))
    } else {
        format!("{}{text}", " ".repeat(missing))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[rstest::rstest]
    #[case::and_first_false("and", json!([1, 0, 2]), json!(0))]
    #[case::and_all_true("and", json!([1, "x"]), json!("x"))]
    #[case::or_first_true("or", json!([0, "", "b", "c"]), json!("b"))]
    #[case::or_all_false("or", json!([0, ""]), json!(""))]
    #[case::not("not", json!([[]]), json!(true))]
    #[case::eq_any("eq", json!(["a", "b", "a"]), json!(true))]
    #[case::eq_none("eq", json!([1, 2, 3]), json!(false))]
    #[case::eq_int_float("eq", json!([1, 1.0]), json!(true))]
    #[case::ne("ne", json!(["a", "b"]), json!(true))]
    #[case::lt("lt", json!([1, 2]), json!(true))]
    #[case::le("le", json!(["b", "a"]), json!(false))]
    #[case::gt("gt", json!([2.5, 2]), json!(true))]
    #[case::ge("ge", json!([2, 2]), json!(true))]
    #[case::index_array("index", json!([["a", "b"], 1]), json!("b"))]
    #[case::index_map("index", json!([{"k": {"n": 7}}, "k", "n"]), json!(7))]
    #[case::index_missing_key("index", json!([{"k": 1}, "z"]), json!(null))]
    #[case::len_string("len", json!(["abc"]), json!(3))]
    #[case::len_array("len", json!([[1, 2]]), json!(2))]
    #[case::len_map("len", json!([{"a": 1}]), json!(1))]
    #[case::slice_string("slice", json!(["abcdef", 1, 3]), json!("bc"))]
    #[case::slice_string_open("slice", json!(["abcdef", 4]), json!("ef"))]
    #[case::slice_array("slice", json!([[1, 2, 3], 0, 2]), json!([1, 2]))]
    #[case::join("join", json!([["a", "b"], ","]), json!("a,b"))]
    #[case::split("split", json!(["a,b", ","]), json!(["a", "b"]))]
    #[case::lower("lower", json!(["AbC"]), json!("abc"))]
    #[case::upper("upper", json!(["AbC"]), json!("ABC"))]
    #[case::title("title", json!(["hello big_world-x"]), json!("Hello Big_world-X"))]
    #[case::truncate("truncate", json!(["abcdef", 3]), json!("abc"))]
    #[case::truncate_short("truncate", json!(["ab", 5]), json!("ab"))]
    #[case::json("json", json!([{"a": [1]}]), json!("{\"a\":[1]}"))]
    #[case::print("print", json!(["a", 1, 2, "b"]), json!("a1 2b"))]
    #[case::println("println", json!(["a", "b"]), json!("a b\n"))]
    #[case::printf_verbs(
        "printf",
        json!(["%s=%d %v %q", "k", 5, [1, 2], "x"]),
        json!("k=5 [1 2] \"x\"")
    )]
    #[case::printf_width(
        "printf",
        json!(["[%5s][%-5s][%05d]", "a", "b", 42]),
        json!("[    a][b    ][00042]")
    )]
    #[case::printf_precision("printf", json!(["%.2f %.1s %%", 1.23456, "xyz"]), json!("1.23 x %"))]
    #[case::printf_missing("printf", json!(["%d"]), json!("%!d(MISSING)"))]
    fn given_a_function_when_called_then_it_matches_go(
        #[case] name: &str,
        #[case] args: Value,
        #[case] expected: Value,
    ) {
        let args = args.as_array().expect("array").clone();

        let result = call(name, &args).expect("call");

        assert_eq!(result, expected);
    }

    #[rstest::rstest]
    #[case::mixed_compare("eq", json!([1, "a"]))]
    #[case::index_out_of_range("index", json!([[1], 4]))]
    #[case::slice_past_end("slice", json!(["ab", 0, 5]))]
    #[case::len_of_number("len", json!([3]))]
    #[case::wrong_arity("lower", json!(["a", "b"]))]
    fn given_bad_arguments_when_called_then_it_fails(#[case] name: &str, #[case] args: Value) {
        let args = args.as_array().expect("array").clone();

        assert!(call(name, &args).is_err());
    }

    #[rstest::rstest]
    #[case::array(json!(["a", 1]), "[a 1]")]
    #[case::map(json!({"b": 2, "a": [true]}), "map[a:[true] b:2]")]
    #[case::null_inside(json!([null]), "[<nil>]")]
    #[case::null_alone(json!(null), "")]
    fn given_a_value_when_displayed_then_it_prints_like_go(
        #[case] value: Value,
        #[case] expected: &str,
    ) {
        assert_eq!(display(&value), expected);
    }
}
