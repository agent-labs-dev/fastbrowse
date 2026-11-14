//! pydantic's lax mode for the scalars models declare, used as `deserialize_with`.
//!
//! JSON from another program reaches a model through `model_validate`, which takes `"3"` and `3.0` for an int and
//! `"yes"` for a bool. serde's own impls refuse both, so a message Python accepted would be refused here. Strings are
//! not coerced from anything: pydantic refuses `3` for a `str` as well. Field bounds (`ge`, `gt`, `le`) are checked
//! here too, since they are part of what the model accepts.

use serde::de::Error;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// An int as pydantic reads one: an int, a float with no fractional part, a bool, or a string holding an int
/// (surrounding whitespace, a sign, `_` separators and a `.0` tail allowed).
pub fn int_of(value: &Value) -> Result<i64, String> {
    match value {
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.2e18 {
                    Ok(f as i64)
                } else {
                    Err("Input should be a valid integer, got a number with a fractional part".into())
                }
            } else {
                Err("Input should be a valid integer".into())
            }
        }
        Value::String(s) => int_from_str(s)
            .ok_or_else(|| "Input should be a valid integer, unable to parse string as an integer".into()),
        _ => Err("Input should be a valid integer".into()),
    }
}

fn int_from_str(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    // "3.0" is an int to pydantic, "3.5" is not.
    let whole = match trimmed.split_once('.') {
        Some((whole, fraction)) if fraction.bytes().all(|b| b == b'0') => whole,
        Some(_) => return None,
        None => trimmed,
    };
    let digits = whole.strip_prefix(['+', '-']).unwrap_or(whole);
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None;
    }
    whole.replace('_', "").parse().ok()
}

/// `float(text)`, which takes surrounding whitespace, a sign and `_` between digits only.
pub fn py_float(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return lower.replace("infinity", "inf").parse().ok();
    }
    let bad_underscore = trimmed.starts_with('_')
        || trimmed.ends_with('_')
        || trimmed.contains("__")
        || trimmed.split('_').skip(1).any(|part| !part.starts_with(|c: char| c.is_ascii_digit()))
        || trimmed.split('_').rev().skip(1).any(|part| !part.ends_with(|c: char| c.is_ascii_digit()));
    if trimmed.is_empty() || bad_underscore || !trimmed.bytes().all(|b| b.is_ascii_digit() || b"+-.eE_".contains(&b)) {
        return None;
    }
    trimmed.replace('_', "").parse().ok()
}

/// A float as pydantic reads one: a number, a bool, or a string Python's `float()` reads.
pub fn float_of(value: &Value) -> Result<f64, String> {
    match value {
        Value::Bool(b) => Ok(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64().ok_or_else(|| "Input should be a valid number".into()),
        Value::String(s) => {
            py_float(s).ok_or_else(|| "Input should be a valid number, unable to parse string as a number".into())
        }
        _ => Err("Input should be a valid number".into()),
    }
}

/// A bool as pydantic reads one: a bool, 0 or 1, or one of its words in any case.
pub fn bool_of(value: &Value) -> Result<bool, String> {
    let invalid = || "Input should be a valid boolean".to_string();
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Ok(false),
            Some(1.0) => Ok(true),
            _ => Err(invalid()),
        },
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(invalid()),
        },
        _ => Err(invalid()),
    }
}

/// pydantic's `StrictInt`: an int and nothing else, not even `true` or `1.0`.
pub fn strict_int_of(value: &Value) -> Result<i64, String> {
    value.as_i64().ok_or_else(|| "Input should be a valid integer".into())
}

fn check<T: PartialOrd + Copy + std::fmt::Display>(value: T, bound: Bound<T>) -> Result<T, String> {
    match bound {
        Bound::Ge(b) if value < b => Err(format!("Input should be greater than or equal to {b}")),
        Bound::Gt(b) if value <= b => Err(format!("Input should be greater than {b}")),
        _ => Ok(value),
    }
}

#[derive(Clone, Copy)]
enum Bound<T> {
    None,
    Ge(T),
    Gt(T),
    /// `ge=0, le=1`, declared on probabilities and confidences.
    Unit,
}

fn de_int<'de, D: Deserializer<'de>>(d: D, bound: Bound<i64>) -> Result<i64, D::Error> {
    let value = Value::deserialize(d)?;
    int_of(&value).and_then(|i| check(i, bound)).map_err(D::Error::custom)
}

fn de_opt_int<'de, D: Deserializer<'de>>(d: D, bound: Bound<i64>) -> Result<Option<i64>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        value => int_of(&value).and_then(|i| check(i, bound)).map(Some).map_err(D::Error::custom),
    }
}

fn de_float<'de, D: Deserializer<'de>>(d: D, bound: Bound<f64>) -> Result<f64, D::Error> {
    let value = Value::deserialize(d)?;
    float_of(&value).and_then(|f| check_float(f, bound)).map_err(D::Error::custom)
}

fn de_opt_float<'de, D: Deserializer<'de>>(d: D, bound: Bound<f64>) -> Result<Option<f64>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        value => float_of(&value).and_then(|f| check_float(f, bound)).map(Some).map_err(D::Error::custom),
    }
}

fn check_float(value: f64, bound: Bound<f64>) -> Result<f64, String> {
    match bound {
        Bound::Unit if (0.0..=1.0).contains(&value) => Ok(value),
        Bound::Unit if value < 0.0 => Err("Input should be greater than or equal to 0".into()),
        Bound::Unit => Err("Input should be less than or equal to 1".into()),
        other => check(value, other),
    }
}

pub fn int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    de_int(d, Bound::None)
}
pub fn int_ge0<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    de_int(d, Bound::Ge(0))
}
pub fn int_gt0<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    de_int(d, Bound::Gt(0))
}
pub fn int_gt1<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    de_int(d, Bound::Gt(1))
}
pub fn opt_int<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    de_opt_int(d, Bound::None)
}
pub fn opt_int_ge0<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    de_opt_int(d, Bound::Ge(0))
}
pub fn opt_int_gt0<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    de_opt_int(d, Bound::Gt(0))
}
pub fn float<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_float(d, Bound::None)
}
pub fn float_gt0<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_float(d, Bound::Gt(0.0))
}
pub fn float_unit<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    de_float(d, Bound::Unit)
}
pub fn opt_float_ge0<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    de_opt_float(d, Bound::Ge(0.0))
}
pub fn opt_float_gt0<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    de_opt_float(d, Bound::Gt(0.0))
}
pub fn opt_float_unit<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    de_opt_float(d, Bound::Unit)
}
pub fn boolean<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    bool_of(&Value::deserialize(d)?).map_err(D::Error::custom)
}
pub fn opt_boolean<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        value => bool_of(&value).map(Some).map_err(D::Error::custom),
    }
}
/// A field declared `T | None` with no default: it may be null, but it has to be there. serde would otherwise
/// fill a missing `Option` with `None`, which pydantic refuses as a missing field.
pub fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

pub fn strict_int<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    strict_int_of(&Value::deserialize(d)?).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_float_takes_underscores_only_between_digits() {
        assert_eq!(float_of(&json!("1_0.5")), Ok(10.5));
        for malformed in ["1__0", "_1", "1_", "1_.5"] {
            assert!(float_of(&json!(malformed)).is_err(), "{malformed}");
        }
    }

    #[test]
    fn ints_are_read_as_pydantic_reads_them() {
        for (input, expected) in [
            (json!(3), Some(3)),
            (json!(3.0), Some(3)),
            (json!(3.5), None),
            (json!("3"), Some(3)),
            (json!(" 3 "), Some(3)),
            (json!("3.0"), Some(3)),
            (json!("3.5"), None),
            (json!(true), Some(1)),
            (json!("1e3"), None),
            (json!(1e3), Some(1000)),
            (json!("0x10"), None),
            (json!("+3"), Some(3)),
            (json!("-0"), Some(0)),
            (json!("1_000"), Some(1000)),
            (json!(null), None),
        ] {
            assert_eq!(int_of(&input).ok(), expected, "{input}");
        }
    }

    #[test]
    fn floats_and_bools_are_read_as_pydantic_reads_them() {
        for (input, expected) in [
            (json!(3), Some(3.0)),
            (json!("1.5"), Some(1.5)),
            (json!(" 1.5 "), Some(1.5)),
            (json!(true), Some(1.0)),
            (json!("1e3"), Some(1000.0)),
            (json!("abc"), None),
            (json!("1_000.5"), Some(1000.5)),
        ] {
            assert_eq!(float_of(&input).ok(), expected, "{input}");
        }
        assert!(float_of(&json!("inf")).unwrap().is_infinite());
        for (input, expected) in [
            (json!(0), Some(false)),
            (json!(1), Some(true)),
            (json!(2), None),
            (json!("True"), Some(true)),
            (json!("yes"), Some(true)),
            (json!("FALSE"), Some(false)),
            (json!("t"), Some(true)),
            (json!(1.0), Some(true)),
            (json!("  true "), None),
        ] {
            assert_eq!(bool_of(&input).ok(), expected, "{input}");
        }
    }
}
