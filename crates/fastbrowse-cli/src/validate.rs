//! What pydantic says is wrong with a message, by field, for the models `serve` reads off the wire.
//!
//! A refusal's text is `_problems(exc)`: every error pydantic found, as `loc: msg`, joined by `; `. serde stops at the
//! first fault and words it its own way, so the shapes are checked here first, in pydantic's order (declared fields,
//! then extra ones), with pydantic's messages. Only a message that passes is deserialized into the core models.

use fastbrowse_core::py::{base64, lax};
use serde_json::{Map, Value};

/// One validation error: where, and pydantic's `msg`.
pub type Problem = (Vec<String>, String);

#[derive(Debug, Clone)]
pub enum Ty {
    Str,
    /// `Field(min_length=1)`.
    NonEmptyStr,
    Int,
    StrictInt,
    PositiveInt,
    Float,
    PositiveFloat,
    Bool,
    Path,
    Base64,
    /// `dict[str, Any]`.
    AnyDict,
    Optional(Box<Ty>),
    /// `tuple[T, ...]`.
    Tuple(Box<Ty>),
    /// `tuple[T, T]`.
    Pair(Box<Ty>),
    /// `dict[str, T]`.
    Dict(Box<Ty>),
    Model(Model),
    /// `Literal["2.0"]`.
    Literal(&'static str),
    /// `StrictInt | StrictStr`.
    RequestId,
    /// `dict[str, Any] | list[Any]`.
    RequestParams,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub name: &'static str,
    pub fields: Vec<(&'static str, Ty, bool)>,
    /// `extra="forbid"`.
    pub closed: bool,
}

fn optional(ty: Ty) -> Ty {
    Ty::Optional(Box::new(ty))
}

fn at(loc: &[String], key: impl ToString) -> Vec<String> {
    let mut loc = loc.to_vec();
    loc.push(key.to_string());
    loc
}

pub fn validate(ty: &Ty, value: &Value, loc: &[String], problems: &mut Vec<Problem>) {
    let mut fail = |message: &str| problems.push((loc.to_vec(), message.to_string()));
    match ty {
        Ty::Str => {
            if !value.is_string() {
                fail("Input should be a valid string");
            }
        }
        Ty::NonEmptyStr => match value {
            Value::String(text) if text.is_empty() => fail("String should have at least 1 character"),
            Value::String(_) => {}
            _ => fail("Input should be a valid string"),
        },
        Ty::Int | Ty::PositiveInt => match int(value) {
            Err(message) => fail(&message),
            Ok(number) if matches!(ty, Ty::PositiveInt) && number <= 0 => fail("Input should be greater than 0"),
            Ok(_) => {}
        },
        Ty::StrictInt => {
            if lax::strict_int_of(value).is_err() {
                fail("Input should be a valid integer");
            }
        }
        Ty::Float | Ty::PositiveFloat => match lax::float_of(value) {
            Err(message) => fail(&message),
            Ok(number) if matches!(ty, Ty::PositiveFloat) && number <= 0.0 => fail("Input should be greater than 0"),
            Ok(_) => {}
        },
        Ty::Bool => {
            if let Err(message) = boolean(value) {
                fail(message);
            }
        }
        Ty::Path => {
            if !value.is_string() {
                fail("Input is not a valid path for <class 'pathlib._local.Path'>");
            }
        }
        Ty::Base64 => match value {
            Value::String(text) => {
                if let Err(error) = base64::decode(text) {
                    fail(&format!("Base64 decoding error: '{error}'"));
                }
            }
            _ => fail("Input should be a valid bytes"),
        },
        Ty::AnyDict => {
            if !value.is_object() {
                fail("Input should be a valid dictionary");
            }
        }
        Ty::Optional(inner) => {
            if !value.is_null() {
                validate(inner, value, loc, problems);
            }
        }
        Ty::Tuple(item) => match value {
            Value::Array(items) => {
                for (index, element) in items.iter().enumerate() {
                    validate(item, element, &at(loc, index), problems);
                }
            }
            _ => fail("Input should be a valid tuple"),
        },
        Ty::Pair(item) => match value {
            Value::Array(items) if items.len() > 2 => {
                fail(&format!("Tuple should have at most 2 items after validation, not {}", items.len()));
            }
            Value::Array(items) => {
                for index in 0..2 {
                    match items.get(index) {
                        Some(element) => validate(item, element, &at(loc, index), problems),
                        None => problems.push((at(loc, index), "Field required".into())),
                    }
                }
            }
            _ => fail("Input should be a valid tuple"),
        },
        Ty::Dict(item) => match value {
            Value::Object(map) => {
                for (key, element) in map {
                    validate(item, element, &at(loc, key), problems);
                }
            }
            _ => fail("Input should be a valid dictionary"),
        },
        Ty::Model(model) => match value {
            Value::Object(map) => validate_model(model, map, loc, problems),
            _ => fail(&format!("Input should be a valid dictionary or instance of {}", model.name)),
        },
        Ty::Literal(expected) => {
            if value.as_str() != Some(expected) {
                fail(&format!("Input should be '{expected}'"));
            }
        }
        Ty::RequestId => {
            if !(value.is_string() || value.is_null() || lax::strict_int_of(value).is_ok()) {
                problems.push((at(loc, "int"), "Input should be a valid integer".into()));
                problems.push((at(loc, "str"), "Input should be a valid string".into()));
            }
        }
        Ty::RequestParams => {
            if !(value.is_object() || value.is_array() || value.is_null()) {
                problems.push((at(loc, "dict[str,any]"), "Input should be a valid dictionary".into()));
                problems.push((at(loc, "list[any]"), "Input should be a valid list".into()));
            }
        }
    }
}

fn validate_model(model: &Model, map: &Map<String, Value>, loc: &[String], problems: &mut Vec<Problem>) {
    for (name, ty, required) in &model.fields {
        match map.get(*name) {
            Some(value) => validate(ty, value, &at(loc, name), problems),
            None if *required => problems.push((at(loc, name), "Field required".into())),
            None => {}
        }
    }
    if model.closed {
        for key in map.keys().filter(|key| !model.fields.iter().any(|(name, _, _)| name == key)) {
            problems.push((at(loc, key), "Extra inputs are not permitted".into()));
        }
    }
}

/// A lax int, with pydantic's words for a float too large to hold.
fn int(value: &Value) -> Result<i64, String> {
    match value.as_f64() {
        Some(number) if !value.is_i64() && !value.is_u64() && number.fract() == 0.0 && number.abs() >= 9.2e18 => {
            Err("Unable to parse input string as an integer, exceeded maximum size".into())
        }
        _ => lax::int_of(value),
    }
}

/// A lax bool, telling apart what pydantic could not read as one from what is no candidate at all.
fn boolean(value: &Value) -> Result<(), &'static str> {
    const UNREADABLE: &str = "Input should be a valid boolean, unable to interpret input";
    match value {
        Value::Bool(_) => Ok(()),
        Value::Number(number) if number.is_i64() || number.is_u64() => {
            if matches!(number.as_i64(), Some(0 | 1)) {
                Ok(())
            } else {
                Err(UNREADABLE)
            }
        }
        Value::String(_) => lax::bool_of(value).map(drop).map_err(|_| UNREADABLE),
        _ => lax::bool_of(value).map(drop).map_err(|_| "Input should be a valid boolean"),
    }
}

/// `_problems(exc)`: each error as `loc: msg`, the whole message named `params` when the fault is its type.
pub fn problems_text(problems: &[Problem]) -> String {
    let lines: Vec<String> = problems
        .iter()
        .map(|(loc, message)| {
            let loc = if loc.is_empty() { "params".to_string() } else { loc.join(".") };
            format!("{loc}: {message}")
        })
        .collect();
    lines.join("; ")
}

/// Validate `value` as `ty` from the top, returning `_problems` text when it fails.
pub fn check(ty: &Ty, value: &Value) -> Result<(), String> {
    let mut problems = Vec::new();
    validate(ty, value, &[], &mut problems);
    if problems.is_empty() { Ok(()) } else { Err(problems_text(&problems)) }
}

fn model(name: &'static str, closed: bool, fields: Vec<(&'static str, Ty, bool)>) -> Ty {
    Ty::Model(Model { name, fields, closed })
}

/// `protocol.Request`.
pub fn request() -> Ty {
    model(
        "Request",
        false,
        vec![
            ("jsonrpc", Ty::Literal("2.0"), false),
            ("id", Ty::RequestId, false),
            ("method", Ty::Str, true),
            ("params", Ty::RequestParams, false),
        ],
    )
}

pub fn no_params() -> Ty {
    model("NoParams", true, vec![])
}

pub fn initialize_params() -> Ty {
    model("InitializeParams", false, vec![("protocol_version", Ty::StrictInt, true)])
}

pub fn run_cancel_params() -> Ty {
    model("RunCancelParams", true, vec![("run_id", Ty::Str, true)])
}

fn limits() -> Ty {
    model(
        "Limits",
        true,
        vec![
            ("max_steps", optional(Ty::PositiveInt), false),
            ("max_jev_calls", optional(Ty::PositiveInt), false),
            ("max_llm_calls", optional(Ty::PositiveInt), false),
            ("max_dollars", optional(Ty::PositiveFloat), false),
            ("max_seconds", optional(Ty::PositiveFloat), false),
        ],
    )
}

pub fn run_params() -> Ty {
    let attachment = model(
        "RunAttachment",
        true,
        vec![("name", Ty::Str, true), ("mime_type", Ty::Str, true), ("content", Ty::Base64, true)],
    );
    let secret_ref =
        model("SecretRef", true, vec![("name", Ty::Str, true), ("origins", Ty::Tuple(Box::new(Ty::Str)), true)]);
    let authorization = model("Authorization", true, vec![("irreversible_actions", Ty::Bool, false)]);
    let chrome = model(
        "LocalChrome",
        true,
        vec![("binary", optional(Ty::Str), false), ("headed", Ty::Bool, false), ("profile", optional(Ty::Path), false)],
    );
    model(
        "RunParams",
        true,
        vec![
            ("run_id", Ty::NonEmptyStr, true),
            ("task", Ty::Str, true),
            ("start", optional(Ty::Str), false),
            ("context", optional(Ty::Str), false),
            ("inputs", optional(Ty::Dict(Box::new(Ty::Str))), false),
            ("attachments", Ty::Tuple(Box::new(attachment)), false),
            ("limits", optional(limits()), false),
            ("authorization", optional(authorization), false),
            ("output_schema", optional(Ty::AnyDict), false),
            ("secrets", Ty::Tuple(Box::new(secret_ref)), false),
            ("frames", Ty::Bool, false),
            ("until", Ty::Bool, false),
            ("downloads", optional(Ty::Path), false),
            ("record", optional(Ty::Path), false),
            ("cursor", optional(Ty::Bool), false),
            ("local", Ty::Bool, false),
            ("chrome", optional(chrome), false),
            ("cloud_profile", optional(Ty::Str), false),
            ("cdp_url", optional(Ty::Str), false),
            ("cdp_port", optional(Ty::Int), false),
            ("attach", Ty::Bool, false),
            ("target_match", optional(Ty::Str), false),
            ("foreground", Ty::Bool, false),
            ("proxy_country", optional(Ty::Str), false),
            ("viewport", optional(Ty::Pair(Box::new(Ty::Int))), false),
            ("cloud_allow_resizing", Ty::Bool, false),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(params: Value) -> String {
        check(&run_params(), &params).err().unwrap_or_else(|| "OK".into())
    }

    #[test]
    fn problems_read_as_pydantic_reports_them() {
        assert_eq!(run(json!({})), "run_id: Field required; task: Field required");
        assert_eq!(run(json!({"run_id": "", "task": "t"})), "run_id: String should have at least 1 character");
        assert_eq!(
            run(json!({"run_id": "r", "task": "t", "zzz": 1, "task2": 2, "limits": {"max_steps": 0}})),
            "limits.max_steps: Input should be greater than 0; zzz: Extra inputs are not permitted; task2: Extra inputs are not permitted"
        );
        assert_eq!(
            run(
                json!({"run_id": "r", "task": "t", "limits": {"max_steps": "x", "max_dollars": -1, "max_seconds": "abc", "max_jev_calls": 1.5}})
            ),
            "limits.max_steps: Input should be a valid integer, unable to parse string as an integer; limits.max_jev_calls: Input should be a valid integer, got a number with a fractional part; limits.max_dollars: Input should be greater than 0; limits.max_seconds: Input should be a valid number, unable to parse string as a number"
        );
        assert_eq!(
            run(json!({"run_id": "r", "task": "t", "chrome": {"headed": "maybe", "binary": 3, "profile": 4}})),
            "chrome.binary: Input should be a valid string; chrome.headed: Input should be a valid boolean, unable to interpret input; chrome.profile: Input is not a valid path for <class 'pathlib._local.Path'>"
        );
        assert_eq!(run(json!({"run_id": "r", "task": "t", "viewport": [1280]})), "viewport.1: Field required");
        assert_eq!(
            run(json!({"run_id": "r", "task": "t", "viewport": [1, 2, 3]})),
            "viewport: Tuple should have at most 2 items after validation, not 3"
        );
        assert_eq!(
            run(json!({"run_id": "r", "task": "t", "frames": 2, "until": 0.5, "cursor": [], "local": {}})),
            "frames: Input should be a valid boolean, unable to interpret input; until: Input should be a valid boolean; cursor: Input should be a valid boolean; local: Input should be a valid boolean"
        );
        assert_eq!(run(json!({"run_id": "r", "task": "t", "frames": "yes", "cdp_port": true, "inputs": null})), "OK");
        assert_eq!(run(json!([])), "params: Input should be a valid dictionary or instance of RunParams");
        assert_eq!(
            run(json!({"run_id": "r", "task": "t", "attachments": [{"name": "a", "mime_type": "m", "content": 5}]})),
            "attachments.0.content: Input should be a valid bytes"
        );
    }

    #[test]
    fn a_request_names_each_member_of_a_union_it_failed() {
        let problems = |value: Value| check(&request(), &value).unwrap_err();
        assert_eq!(problems(json!({"jsonrpc": "1.0", "id": 4, "method": "x"})), "jsonrpc: Input should be '2.0'");
        assert_eq!(
            problems(json!({"id": true, "method": "x", "params": 5})),
            "id.int: Input should be a valid integer; id.str: Input should be a valid string; params.dict[str,any]: Input should be a valid dictionary; params.list[any]: Input should be a valid list"
        );
        assert_eq!(problems(json!("x")), "params: Input should be a valid dictionary or instance of Request");
    }
}
