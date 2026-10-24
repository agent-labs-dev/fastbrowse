//! JSON text exactly as the Python side writes it.
//!
//! Three writers cross a seam, and each has its own bytes:
//!
//! - [`Style::DUMPS`]: `json.dumps` with its defaults, `", "` and `": "` separators and `ensure_ascii=True`. Prompts,
//!   cache keys and digests are built this way.
//! - [`Style::HTTPX`]: what httpx sends for `json=`, compact and UTF-8. Every model request body is written so.
//! - [`Style::PYDANTIC`]: `model_dump_json`, compact and UTF-8, with pydantic-core's own float format. The
//!   protocol's messages and model fields quoted into prompts are written so.
//!
//! A replayed request that differs from the recorded one by a byte is a miss, so the float formats are kept apart
//! rather than shared: `1e-05` from Python's `repr`, `0.00001` from pydantic-core.

use serde_json::Value;

/// How floats are spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Floats {
    /// `repr(float)`: positional for exponents -4 through 15, otherwise `1e-05` and `1e+16`.
    Repr,
    /// pydantic-core: positional for exponents -5 through 15, otherwise `1e-7` and `1e+16`.
    Pydantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub item_separator: &'static str,
    pub key_separator: &'static str,
    pub ensure_ascii: bool,
    pub sort_keys: bool,
    pub floats: Floats,
    /// Spaces per level; `None` writes everything on one line.
    pub indent: Option<usize>,
}

impl Style {
    pub const DUMPS: Style = Style {
        item_separator: ", ",
        key_separator: ": ",
        ensure_ascii: true,
        sort_keys: false,
        floats: Floats::Repr,
        indent: None,
    };
    pub const HTTPX: Style = Style {
        item_separator: ",",
        key_separator: ":",
        ensure_ascii: false,
        sort_keys: false,
        floats: Floats::Repr,
        indent: None,
    };
    pub const PYDANTIC: Style = Style {
        item_separator: ",",
        key_separator: ":",
        ensure_ascii: false,
        sort_keys: false,
        floats: Floats::Pydantic,
        indent: None,
    };

    /// `json.dumps(..., sort_keys=True)`.
    pub const fn sorted(self) -> Style {
        Style { sort_keys: true, ..self }
    }

    /// `json.dumps(..., ensure_ascii=False)`.
    pub const fn unicode(self) -> Style {
        Style { ensure_ascii: false, ..self }
    }

    /// `model_dump_json(indent=n)`, whose item separator is `,` before the newline.
    pub const fn indented(self, spaces: usize) -> Style {
        Style { indent: Some(spaces), item_separator: ",", key_separator: ": ", ..self }
    }
}

/// `json.dumps(value)`.
pub fn dumps(value: &Value) -> String {
    write(value, Style::DUMPS)
}

/// `model_dump_json()` of whatever `value` was dumped from.
pub fn dump_json(value: &Value) -> String {
    write(value, Style::PYDANTIC)
}

pub fn write(value: &Value, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, value, style, 0);
    out
}

fn write_value(out: &mut String, value: &Value, style: Style, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                out.push_str(&int.to_string());
            } else if let Some(int) = number.as_u64() {
                out.push_str(&int.to_string());
            } else {
                out.push_str(&float(number.as_f64().unwrap_or(f64::NAN), style.floats));
            }
        }
        Value::String(text) => write_string(out, text, style.ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(style.item_separator);
                }
                newline(out, style, depth + 1);
                write_value(out, item, style, depth + 1);
            }
            newline(out, style, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(style.item_separator);
                }
                newline(out, style, depth + 1);
                write_string(out, key, style.ensure_ascii);
                out.push_str(style.key_separator);
                write_value(out, item, style, depth + 1);
            }
            newline(out, style, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, style: Style, depth: usize) {
    if let Some(spaces) = style.indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', spaces * depth));
    }
}

/// A JSON string literal. Both Python writers escape `"`, `\` and C0 controls (with `\n`, `\r`, `\t`, `\b`, `\f`
/// by name); `ensure_ascii` also escapes everything outside printable ASCII, DEL included, as UTF-16 units.
pub fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (ensure_ascii && !(' '..='~').contains(&c)) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The length of `text` as `json.dumps` writes it, quotes excluded, in Python characters.
pub fn dumped_len(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            ' '..='~' => 1,
            c if c.len_utf16() == 2 => 12,
            _ => 6,
        })
        .sum()
}

/// A float as `repr` or pydantic-core spells it. Non-finite values are `Infinity`/`NaN` as `json.dumps` writes them
/// (pydantic writes `null`, which a `Value` already holds by then).
pub fn float(value: f64, style: Floats) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Rust's `{:e}` is the shortest representation that round-trips, the same digits CPython's repr and ryu pick.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let positional = match style {
        Floats::Repr => (-4..16).contains(&exponent),
        Floats::Pydantic => (-5..16).contains(&exponent),
    };
    let body = if positional {
        let point = exponent + 1;
        let count = digits.len() as i32;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point >= count {
            format!("{digits}{}.0", "0".repeat((point - count) as usize))
        } else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        }
    } else {
        let mantissa = if digits.len() == 1 { digits.clone() } else { format!("{}.{}", &digits[..1], &digits[1..]) };
        let magnitude = exponent.unsigned_abs();
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        match style {
            Floats::Repr => format!("{mantissa}e{exponent_sign}{magnitude:02}"),
            Floats::Pydantic => format!("{mantissa}e{exponent_sign}{magnitude}"),
        }
    };
    format!("{sign}{body}")
}

/// `str(value)` for a float, which is its `repr`.
pub fn float_str(value: f64) -> String {
    match value {
        v if v.is_nan() => "nan".into(),
        v if v.is_infinite() => if v > 0.0 { "inf" } else { "-inf" }.into(),
        v => float(v, Floats::Repr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_follow_repr_and_pydantic_each() {
        for (value, repr, pydantic) in [
            (1e16, "1e+16", "1e+16"),
            (1e-7, "1e-07", "1e-7"),
            (0.1, "0.1", "0.1"),
            (1.0, "1.0", "1.0"),
            (123456789012345678.0, "1.2345678901234568e+17", "1.2345678901234568e+17"),
            (0.0001, "0.0001", "0.0001"),
            (0.00001, "1e-05", "0.00001"),
            (-0.0, "-0.0", "-0.0"),
            (1e300, "1e+300", "1e+300"),
            (5e-324, "5e-324", "5e-324"),
            (1e15, "1000000000000000.0", "1000000000000000.0"),
            (12.5, "12.5", "12.5"),
        ] {
            assert_eq!(float(value, Floats::Repr), repr, "{value}");
            assert_eq!(float(value, Floats::Pydantic), pydantic, "{value}");
        }
    }

    #[test]
    fn dumps_escapes_as_python_does() {
        let value = json!({"a": ["\u{e9}\u{2028}</\u{7f}\u{1}\u{1f600}", 1, 1.5, null, true]});
        let escaped = ["u00e9", "u2028</", "u007f", "u0001", "ud83d", "ude00"].join("\\");
        assert_eq!(dumps(&value), format!("{{\"a\": [\"\\{escaped}\", 1, 1.5, null, true]}}"));
        assert_eq!(dump_json(&value), "{\"a\":[\"\u{e9}\u{2028}</\u{7f}\\u0001\u{1f600}\",1,1.5,null,true]}");
    }

    #[test]
    fn an_indented_dump_matches_pydantic() {
        let value = json!({"a": [1, {}], "b": []});
        assert_eq!(write(&value, Style::PYDANTIC.indented(2)), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
    }
}
