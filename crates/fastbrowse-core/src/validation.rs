//! pydantic's `model_validate_json` over a [`Ty`] tree, failing with the error text pydantic writes.
//!
//! A client that cannot validate a structured response asks the model again with
//! `ValidationError.json(include_input=False, include_url=False)`. That text goes into the next request body, and a
//! run is proven by replaying recorded model traffic keyed on a hash of that body, so a re-ask only replays when its
//! errors carry pydantic's types, locations, messages and order byte for byte. serde's own messages never would.
//!
//! Validation is pydantic's lax mode on JSON input: `"3"` and `3.0` are ints, `"yes"` is a bool, nothing becomes a
//! string. Within a model, pydantic-core reports the extra keys a closed model refuses first, in input order, then
//! each field in declaration order. A tuple's `max_length` stops at the first item past the cap and reports that alone,
//! counting the whole input; its `min_length` is checked after every item and counts only the items that validated.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::llm::ResponseSchema;
use crate::py::dump::Dump;
use crate::py::json::{Style, scalar_repr, write};
use crate::py::{lax, text};
use crate::schema::{ModelTy, Schema, Ty, json_schema};

/// One error as `ValidationError.json(include_input=False, include_url=False)` writes it.
pub fn line_error(kind: &str, loc: Vec<Value>, msg: &str, ctx: Option<Value>) -> Value {
    let mut error = Map::new();
    error.insert("type".into(), kind.into());
    error.insert("loc".into(), Value::Array(loc));
    error.insert("msg".into(), msg.into());
    if let Some(ctx) = ctx {
        error.insert("ctx".into(), ctx);
    }
    Value::Object(error)
}

/// The errors as `ValidationError.json()` writes them: compact, UTF-8, through pydantic-core's own writer.
pub fn errors_json(errors: Vec<Value>) -> String {
    write(&Value::Array(errors), Style::PYDANTIC)
}

/// A body jiter could not parse, as pydantic reports it.
///
/// jiter words most failures as serde_json does, position included. Where they part, it is jiter's wording that a
/// recorded re-ask holds: a literal cut short at the end of `text` (`tru`, `nul`) is to jiter the end of the input,
/// while a bad word inside it (`not json`) is a bad identifier to both.
pub fn json_invalid(text: &str, error: &serde_json::Error) -> Value {
    let mut reason = error.to_string();
    let last = text.rsplit('\n').next().unwrap_or_default().chars().count();
    let at_end = error.line() >= text.split('\n').count() && error.column() >= last;
    if let Some(position) = reason.strip_prefix("expected ident").filter(|_| at_end) {
        reason = format!("EOF while parsing a value{position}");
    }
    line_error("json_invalid", vec![], &format!("Invalid JSON: {reason}"), Some(json!({"error": reason})))
}

/// `TypeAdapter(ty).validate_python(value)` as JSON mode reads it: the value `model_dump(mode="json")` would write,
/// fields under their Python names, or every error in pydantic's order.
pub fn validate(ty: &Ty, value: &Value) -> Result<Value, Vec<Value>> {
    Validator { by_alias: false }.run(ty, value)
}

/// `model_validate_json(text)`, failing with `errors_json` of what pydantic reports.
pub fn validate_json(ty: &Ty, text: &str) -> Result<Value, String> {
    let value = parse(text)?;
    validate(ty, &value).map_err(errors_json)
}

fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str::<Value>(text).map_err(|error| errors_json(vec![json_invalid(text, &error)]))
}

/// `T.model_validate_json(text)` for a Rust type: the type's own tree decides what pydantic would refuse, and serde
/// then reads the value pydantic would have accepted. What serde refuses after that is a model validator.
pub fn validate_typed<T: Schema + DeserializeOwned>(text: &str) -> Result<T, String> {
    let value = parse(text)?;
    let checked = Validator { by_alias: true }.run(&T::ty(), &value).map_err(errors_json)?;
    serde_json::from_value::<T>(checked).map_err(|error| errors_json(vec![value_error(&error.to_string())]))
}

/// A `model_validator` that raised `ValueError`: pydantic reports it at the model, the message prefixed.
fn value_error(message: &str) -> Value {
    let reason = message.strip_prefix("Value error, ").unwrap_or(message);
    line_error("value_error", vec![], &format!("Value error, {reason}"), Some(json!({ "error": reason })))
}

/// A model built at run time, Python's `create_model`: its name and schema on the wire, its validation pydantic's.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSchema {
    pub ty: Ty,
}

impl ModelSchema {
    pub fn new(model: ModelTy) -> ModelSchema {
        ModelSchema { ty: model.ty() }
    }
}

impl ResponseSchema for ModelSchema {
    fn name(&self) -> String {
        match &self.ty {
            Ty::Model(model) => model.name.clone(),
            _ => "?".into(),
        }
    }

    fn json_schema(&self) -> Value {
        json_schema(&self.ty)
    }

    fn validate_json(&self, text: &str) -> Result<Value, String> {
        validate_json(&self.ty, text)
    }
}

/// `Typed<T>::validate_json`: the dump of the value pydantic accepts.
pub(crate) fn typed_json<T: Schema + DeserializeOwned + Serialize>(text: &str) -> Result<Value, String> {
    validate_typed::<T>(text).map(|data| data.dump())
}

/// An error below the value being validated, its location relative to that value.
struct Issue {
    kind: &'static str,
    loc: Vec<Value>,
    msg: String,
    ctx: Option<Value>,
}

impl Issue {
    fn new(kind: &'static str, msg: impl Into<String>) -> Issue {
        Issue { kind, loc: Vec::new(), msg: msg.into(), ctx: None }
    }

    fn ctx(mut self, ctx: Value) -> Issue {
        self.ctx = Some(ctx);
        self
    }

    fn at(mut self, key: Value) -> Issue {
        self.loc.insert(0, key);
        self
    }

    fn written(self) -> Value {
        line_error(self.kind, self.loc, &self.msg, self.ctx)
    }
}

type Checked = Result<Value, Vec<Issue>>;

fn fail(issue: Issue) -> Checked {
    Err(vec![issue])
}

struct Validator {
    /// Write fields under their JSON keys, for serde to read back, rather than their Python names as a dump does.
    by_alias: bool,
}

impl Validator {
    fn run(&self, ty: &Ty, value: &Value) -> Result<Value, Vec<Value>> {
        self.check(ty, value).map_err(|issues| issues.into_iter().map(Issue::written).collect())
    }

    fn check(&self, ty: &Ty, value: &Value) -> Checked {
        match ty {
            Ty::Str => string(value),
            Ty::Int => int(value),
            Ty::Float => float(value),
            Ty::Bool => boolean(value),
            Ty::Null => match value {
                Value::Null => Ok(Value::Null),
                _ => fail(Issue::new("none_required", "Input should be None")),
            },
            Ty::Any | Ty::JsonValue => Ok(value.clone()),
            // No response model carries these; a string is taken as written rather than reparsed.
            Ty::DateTime => scalar_string(value, "datetime_type", "Input should be a valid datetime"),
            Ty::Bytes | Ty::Base64 | Ty::Image => scalar_string(value, "bytes_type", "Input should be a valid bytes"),
            Ty::Path => scalar_string(value, "path_type", "Input is not a valid path"),
            Ty::List(item) => self.items(item, value, &[], "tuple_type", "Input should be a valid array"),
            Ty::Set(item) => self.items(item, value, &[], "frozen_set_type", "Input should be a valid frozenset"),
            Ty::Tuple(items) => self.tuple(items, value),
            Ty::Map(inner) => self.map(inner, value),
            Ty::Optional(inner) => match value {
                Value::Null => Ok(Value::Null),
                other => self.check(inner, other),
            },
            Ty::Union(options) => self.union(options, value),
            Ty::Literal(values) => literal(values, value),
            Ty::Enum(definition) => {
                if definition.values.iter().any(|allowed| same(allowed, value)) {
                    Ok(value.clone())
                } else {
                    let expected = expected(&definition.values);
                    fail(Issue::new("enum", format!("Input should be {expected}")).ctx(json!({"expected": expected})))
                }
            }
            Ty::Model(model) => self.model(model, value),
            Ty::Alias(_, inner) => self.check(inner, value),
            Ty::Tagged { property, variants } => self.tagged(property, variants, value),
            Ty::Constrained(inner, keywords) => self.constrained(inner, keywords, value),
        }
    }

    fn model(&self, model: &ModelTy, value: &Value) -> Checked {
        let Value::Object(input) = value else {
            return fail(Issue::new("model_type", "Input should be an object").ctx(json!({"class_name": model.name})));
        };
        let mut issues = Vec::new();
        if model.forbid_extra {
            for key in input.keys() {
                if !model.fields.iter().any(|field| field.key() == key) {
                    issues.push(Issue::new("extra_forbidden", "Extra inputs are not permitted").at(key.clone().into()));
                }
            }
        }
        let mut output = Map::new();
        for field in &model.fields {
            let key = field.key();
            let name = if self.by_alias { key } else { field.name.as_str() };
            match input.get(key) {
                Some(item) => match self.check(&field.ty, item) {
                    Ok(checked) => {
                        output.insert(name.to_string(), checked);
                    }
                    Err(found) => issues.extend(found.into_iter().map(|issue| issue.at(key.into()))),
                },
                None => match &field.default {
                    Some(default) => {
                        output.insert(name.to_string(), default.clone());
                    }
                    // A default factory builds an empty container; the schema writes no default for it.
                    None if field.factory => {
                        let empty = match &field.ty {
                            Ty::Map(_) => Value::Object(Map::new()),
                            _ => Value::Array(Vec::new()),
                        };
                        output.insert(name.to_string(), empty);
                    }
                    None => issues.push(Issue::new("missing", "Field required").at(key.into())),
                },
            }
        }
        if issues.is_empty() { Ok(Value::Object(output)) } else { Err(issues) }
    }

    fn map(&self, inner: &Ty, value: &Value) -> Checked {
        let Value::Object(input) = value else {
            return fail(Issue::new("dict_type", "Input should be an object"));
        };
        let mut issues = Vec::new();
        let mut output = Map::new();
        for (key, item) in input {
            match self.check(inner, item) {
                Ok(checked) => {
                    output.insert(key.clone(), checked);
                }
                Err(found) => issues.extend(found.into_iter().map(|issue| issue.at(key.clone().into()))),
            }
        }
        if issues.is_empty() { Ok(Value::Object(output)) } else { Err(issues) }
    }

    /// A variable-length tuple and the length bounds declared on it.
    fn items(&self, item: &Ty, value: &Value, bounds: &[(&str, &Value)], kind: &'static str, msg: &str) -> Checked {
        let Value::Array(input) = value else {
            return fail(Issue::new(kind, msg));
        };
        let bound = |name: &str| bounds.iter().find(|(keyword, _)| *keyword == name).and_then(|(_, b)| b.as_u64());
        let (min, max) = (bound("minItems"), bound("maxItems"));
        let mut issues = Vec::new();
        let mut output = Vec::new();
        for (index, element) in input.iter().enumerate() {
            match self.check(item, element) {
                Ok(checked) => {
                    output.push(checked);
                    if let Some(max) = max.filter(|max| output.len() as u64 > *max) {
                        return fail(length("too_long", "most", max, input.len()));
                    }
                }
                Err(found) => issues.extend(found.into_iter().map(|issue| issue.at(index.into()))),
            }
        }
        if let Some(min) = min.filter(|min| (output.len() as u64) < *min) {
            issues.push(length("too_short", "least", min, output.len()));
        }
        if issues.is_empty() { Ok(Value::Array(output)) } else { Err(issues) }
    }

    fn tuple(&self, items: &[Ty], value: &Value) -> Checked {
        let Value::Array(input) = value else {
            return fail(Issue::new("tuple_type", "Input should be a valid array"));
        };
        let mut issues = Vec::new();
        let mut output = Vec::new();
        for (index, item) in items.iter().enumerate() {
            match input.get(index) {
                Some(element) => match self.check(item, element) {
                    Ok(checked) => output.push(checked),
                    Err(found) => issues.extend(found.into_iter().map(|issue| issue.at(index.into()))),
                },
                None => issues.push(Issue::new("missing", "Field required").at(index.into())),
            }
        }
        if input.len() > items.len() {
            issues.push(length("too_long", "most", items.len() as u64, input.len()));
        }
        if issues.is_empty() { Ok(Value::Array(output)) } else { Err(issues) }
    }

    /// pydantic's smart union: a branch the value already is wins, then the first that takes it; otherwise each
    /// branch's errors under the branch's tag.
    fn union(&self, options: &[Ty], value: &Value) -> Checked {
        if let Some(option) = options.iter().find(|option| exact(option, value))
            && let Ok(checked) = self.check(option, value)
        {
            return Ok(checked);
        }
        let mut issues = Vec::new();
        for option in options {
            match self.check(option, value) {
                Ok(checked) => return Ok(checked),
                Err(found) => issues.extend(found.into_iter().map(|issue| issue.at(tag(option).into()))),
            }
        }
        Err(issues)
    }

    fn tagged(&self, property: &str, variants: &[(&str, Ty)], value: &Value) -> Checked {
        let discriminator = format!("'{property}'");
        let Some(found) = value.as_object().and_then(|object| object.get(property)) else {
            return fail(
                Issue::new("union_tag_not_found", format!("Unable to extract tag using discriminator {discriminator}"))
                    .ctx(json!({ "discriminator": discriminator })),
            );
        };
        let tag = found.as_str().unwrap_or_default();
        match variants.iter().find(|(name, _)| *name == tag) {
            Some((name, variant)) => self
                .check(variant, value)
                .map_err(|issues| issues.into_iter().map(|issue| issue.at((*name).into())).collect()),
            None => {
                let tags: Vec<String> = variants.iter().map(|(name, _)| format!("'{name}'")).collect();
                let tags = tags.join(", ");
                fail(
                    Issue::new(
                        "union_tag_invalid",
                        format!(
                            "Input tag '{tag}' found using {discriminator} does not match any of the expected tags: \
                             {tags}"
                        ),
                    )
                    .ctx(json!({"discriminator": discriminator, "tag": tag, "expected_tags": tags})),
                )
            }
        }
    }

    fn constrained(&self, inner: &Ty, keywords: &[(&'static str, Value)], value: &Value) -> Checked {
        let bounds: Vec<(&str, &Value)> = keywords.iter().map(|(keyword, bound)| (*keyword, bound)).collect();
        match inner {
            Ty::List(item) => return self.items(item, value, &bounds, "tuple_type", "Input should be a valid array"),
            Ty::Set(item) => {
                return self.items(item, value, &bounds, "frozen_set_type", "Input should be a valid frozenset");
            }
            _ => {}
        }
        let checked = self.check(inner, value)?;
        match &checked {
            Value::String(written) => {
                let count = text::len(written) as u64;
                for (keyword, bound) in &bounds {
                    let Some(limit) = bound.as_u64() else { continue };
                    let unit = if limit == 1 { "character" } else { "characters" };
                    if *keyword == "minLength" && count < limit {
                        return fail(
                            Issue::new("string_too_short", format!("String should have at least {limit} {unit}"))
                                .ctx(json!({ "min_length": limit })),
                        );
                    }
                    if *keyword == "maxLength" && count > limit {
                        return fail(
                            Issue::new("string_too_long", format!("String should have at most {limit} {unit}"))
                                .ctx(json!({ "max_length": limit })),
                        );
                    }
                }
            }
            Value::Number(number) => {
                let float = matches!(inner, Ty::Float);
                // pydantic-core checks the upper bounds before the lower ones.
                for keyword in ["maximum", "exclusiveMaximum", "minimum", "exclusiveMinimum"] {
                    let Some((_, bound)) = bounds.iter().find(|(name, _)| *name == keyword) else { continue };
                    let (Some(given), Some(limit)) = (number.as_f64(), bound.as_f64()) else { continue };
                    let (holds, kind, words, name) = match keyword {
                        "maximum" => (given <= limit, "less_than_equal", "less than or equal to", "le"),
                        "exclusiveMaximum" => (given < limit, "less_than", "less than", "lt"),
                        "minimum" => (given >= limit, "greater_than_equal", "greater than or equal to", "ge"),
                        _ => (given > limit, "greater_than", "greater than", "gt"),
                    };
                    if !holds {
                        // The bound is held in the field's own type, so a float field reports `1.0` where its schema
                        // says 1; the message prints it as Rust's Display does, which drops the `.0`.
                        let ctx_bound = if float { json!(limit) } else { (*bound).clone() };
                        let shown = if float { format!("{limit}") } else { bound.to_string() };
                        let mut ctx = Map::new();
                        ctx.insert(name.into(), ctx_bound);
                        return fail(
                            Issue::new(kind, format!("Input should be {words} {shown}")).ctx(Value::Object(ctx)),
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(checked)
    }
}

fn length(kind: &'static str, side: &str, limit: u64, actual: usize) -> Issue {
    let unit = if limit == 1 { "item" } else { "items" };
    let field = if kind == "too_long" { "max_length" } else { "min_length" };
    let mut ctx = Map::new();
    ctx.insert("field_type".into(), "Tuple".into());
    ctx.insert(field.into(), limit.into());
    ctx.insert("actual_length".into(), actual.into());
    Issue::new(kind, format!("Tuple should have at {side} {limit} {unit} after validation, not {actual}"))
        .ctx(Value::Object(ctx))
}

fn string(value: &Value) -> Checked {
    match value {
        Value::String(_) => Ok(value.clone()),
        _ => fail(Issue::new("string_type", "Input should be a valid string")),
    }
}

fn scalar_string(value: &Value, kind: &'static str, msg: &str) -> Checked {
    match value {
        Value::String(_) => Ok(value.clone()),
        _ => fail(Issue::new(kind, msg)),
    }
}

fn int(value: &Value) -> Checked {
    match value {
        Value::Number(number) if number.as_i64().is_none() && number.as_u64().is_none() => {
            let given = number.as_f64().unwrap_or(f64::NAN);
            if given.fract() != 0.0 {
                fail(Issue::new(
                    "int_from_float",
                    "Input should be a valid integer, got a number with a fractional part",
                ))
            } else if given.abs() >= 9.223_372_036_854_776e18 {
                fail(Issue::new(
                    "int_parsing_size",
                    "Unable to parse input string as an integer, exceeded maximum size",
                ))
            } else {
                Ok(json!(given as i64))
            }
        }
        Value::Number(number) if number.as_i64().is_none() => Ok(value.clone()),
        Value::Bool(_) | Value::Number(_) | Value::String(_) => match lax::int_of(value) {
            Ok(checked) => Ok(json!(checked)),
            Err(_) => {
                fail(Issue::new("int_parsing", "Input should be a valid integer, unable to parse string as an integer"))
            }
        },
        _ => fail(Issue::new("int_type", "Input should be a valid integer")),
    }
}

fn float(value: &Value) -> Checked {
    match value {
        Value::Null | Value::Array(_) | Value::Object(_) => {
            fail(Issue::new("float_type", "Input should be a valid number"))
        }
        _ => match lax::float_of(value) {
            Ok(checked) => Ok(json!(checked)),
            Err(_) => {
                fail(Issue::new("float_parsing", "Input should be a valid number, unable to parse string as a number"))
            }
        },
    }
}

fn boolean(value: &Value) -> Checked {
    let parsing = || Issue::new("bool_parsing", "Input should be a valid boolean, unable to interpret input");
    match value {
        Value::Bool(_) => Ok(value.clone()),
        // An int other than 0 or 1 cannot be read as a bool; a float that is neither is the wrong type altogether.
        Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(0), _) => Ok(false.into()),
            (Some(1), _) => Ok(true.into()),
            (Some(_), _) => fail(parsing()),
            (None, Some(0.0)) => Ok(false.into()),
            (None, Some(1.0)) => Ok(true.into()),
            _ => fail(Issue::new("bool_type", "Input should be a valid boolean")),
        },
        Value::String(_) => lax::bool_of(value).map(Value::from).map_err(|_| vec![parsing()]),
        _ => fail(Issue::new("bool_type", "Input should be a valid boolean")),
    }
}

fn literal(values: &[Value], value: &Value) -> Checked {
    if values.iter().any(|allowed| same(allowed, value)) {
        return Ok(value.clone());
    }
    let expected = expected(values);
    fail(Issue::new("literal_error", format!("Input should be {expected}")).ctx(json!({ "expected": expected })))
}

/// JSON equality as a literal compares: `1` is not `true`, `1.0` is `1`, and integers past 2^53 stay distinct.
fn same(allowed: &Value, value: &Value) -> bool {
    match (allowed, value) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => a.as_f64() == b.as_f64(),
        (a, b) => a == b,
    }
}

/// The allowed values as pydantic lists them: each as its Python repr, the last joined with "or".
fn expected(values: &[Value]) -> String {
    let shown: Vec<String> = values.iter().map(scalar_repr).collect();
    text::or_list(&shown)
}

/// Whether the value already has the branch's type, which a smart union prefers to any coercion.
fn exact(option: &Ty, value: &Value) -> bool {
    match (option, value) {
        (Ty::Str, Value::String(_)) | (Ty::Bool, Value::Bool(_)) | (Ty::Null, Value::Null) => true,
        (Ty::Int, Value::Number(n)) => n.is_i64() || n.is_u64(),
        (Ty::Float, Value::Number(n)) => n.is_f64(),
        (Ty::Model(_) | Ty::Map(_), Value::Object(_)) | (Ty::List(_) | Ty::Tuple(_), Value::Array(_)) => true,
        (Ty::Constrained(inner, _) | Ty::Alias(_, inner), _) => exact(inner, value),
        _ => false,
    }
}

/// The label pydantic puts in an error's location for a union branch.
fn tag(option: &Ty) -> String {
    match option {
        Ty::Str => "str".into(),
        Ty::Int => "int".into(),
        Ty::Float => "float".into(),
        Ty::Bool => "bool".into(),
        Ty::Null => "none".into(),
        Ty::Model(model) => model.name.clone(),
        Ty::Enum(definition) => definition.name.into(),
        Ty::Constrained(inner, _) => format!("constrained-{}", tag(inner)),
        Ty::List(item) => format!("tuple[{}, ...]", tag(item)),
        Ty::Map(value) => format!("dict[str,{}]", tag(value)),
        Ty::Literal(values) => format!("literal[{}]", expected(values).replace(" or ", ",")),
        _ => "any".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;

    fn cite() -> Ty {
        ModelTy::frozen("_Cite", vec![Field::required("first", Ty::Str), Field::required("last", Ty::Str)]).ty()
    }

    #[test]
    fn a_literal_compares_large_integers_exactly() {
        assert!(!same(&json!(9_007_199_254_740_992_i64), &json!(9_007_199_254_740_993_i64)));
        assert!(same(&json!(1), &json!(1.0)));
        assert!(!same(&json!(1), &json!(true)));
    }

    /// `retrieval._ReadClaim` and `_ReadResponse`, cut to what the error cases reach.
    fn read_response() -> Ty {
        let claim = ModelTy::frozen(
            "_ReadClaim",
            vec![
                Field::required("cite", Ty::optional(cite())),
                Field::default(
                    "excerpt",
                    Ty::optional(Ty::Str.constrained("maxLength", 2000).constrained("minLength", 1)),
                    Value::Null,
                ),
                Field::default("draws_on", Ty::list(Ty::Str), json!([])),
                Field::default("records", Ty::list(cite()), json!([])),
                Field::default("orders_list", Ty::optional(cite()), Value::Null),
                Field::required("text", Ty::Str),
            ],
        )
        .ty();
        ModelTy::frozen(
            "_ReadResponse",
            vec![Field::required("claims", Ty::list(claim)), Field::required("answered", Ty::Bool)],
        )
        .ty()
    }

    #[test]
    fn a_read_response_fails_as_pydantic_words_it() {
        let ty = read_response();
        assert_eq!(
            validate_json(
                &ty,
                r#"{"claims": [{"cite": {"first": "a"}, "text": 5, "draws_on": "e1"}], "answered": true}"#
            )
            .unwrap_err(),
            r#"[{"type":"missing","loc":["claims",0,"cite","last"],"msg":"Field required"},{"type":"tuple_type","loc":["claims",0,"draws_on"],"msg":"Input should be a valid array"},{"type":"string_type","loc":["claims",0,"text"],"msg":"Input should be a valid string"}]"#
        );
        assert_eq!(
            validate_json(&ty, r#"{"answered": true, "summary": "x"}"#).unwrap_err(),
            r#"[{"type":"extra_forbidden","loc":["summary"],"msg":"Extra inputs are not permitted"},{"type":"missing","loc":["claims"],"msg":"Field required"}]"#
        );
        assert_eq!(
            validate_json(&ty, r#"{"claims": [{"cite": null, "text": "t", "excerpt": ""}], "answered": true}"#)
                .unwrap_err(),
            r#"[{"type":"string_too_short","loc":["claims",0,"excerpt"],"msg":"String should have at least 1 character","ctx":{"min_length":1}}]"#
        );
        assert_eq!(
            validate_json(&ty, r#"{"claims": ["#).unwrap_err(),
            r#"[{"type":"json_invalid","loc":[],"msg":"Invalid JSON: EOF while parsing a list at line 1 column 12","ctx":{"error":"EOF while parsing a list at line 1 column 12"}}]"#
        );
    }

    #[test]
    fn a_valid_read_response_is_dumped_with_its_defaults() {
        let checked =
            validate_json(&read_response(), r#"{"claims": [{"cite": null, "text": "t"}], "answered": "yes"}"#);
        assert_eq!(
            checked.unwrap(),
            json!({"claims": [{"cite": null, "excerpt": null, "draws_on": [], "records": [], "orders_list": null,
                "text": "t"}], "answered": true})
        );
    }

    #[test]
    fn scalars_are_read_laxly() {
        assert_eq!(validate(&Ty::Int, &json!("1.0")), Ok(json!(1)));
        assert_eq!(validate(&Ty::Int, &json!(1e3)), Ok(json!(1000)));
        assert_eq!(validate(&Ty::Int, &json!(true)), Ok(json!(1)));
        assert_eq!(validate(&Ty::Float, &json!(" 1.5 ")), Ok(json!(1.5)));
        assert_eq!(validate(&Ty::Float, &json!(2)), Ok(json!(2.0)));
        assert_eq!(validate(&Ty::Bool, &json!("TRUE")), Ok(json!(true)));
        assert_eq!(validate(&Ty::Bool, &json!(0.0)), Ok(json!(false)));
        let kind = |ty: &Ty, value: Value| validate(ty, &value).unwrap_err()[0]["type"].clone();
        assert_eq!(kind(&Ty::Int, json!("1e3")), "int_parsing");
        assert_eq!(kind(&Ty::Int, json!(1.5)), "int_from_float");
        assert_eq!(kind(&Ty::Int, json!(1e20)), "int_parsing_size");
        assert_eq!(kind(&Ty::Int, json!(null)), "int_type");
        assert_eq!(kind(&Ty::Float, json!("x")), "float_parsing");
        assert_eq!(kind(&Ty::Float, json!(null)), "float_type");
        assert_eq!(kind(&Ty::Bool, json!(2)), "bool_parsing");
        assert_eq!(kind(&Ty::Bool, json!(1.5)), "bool_type");
        assert_eq!(kind(&Ty::Bool, json!(" true")), "bool_parsing");
        assert_eq!(kind(&Ty::Str, json!(1)), "string_type");
    }

    #[test]
    fn a_tuple_cap_reports_the_whole_input_alone() {
        let capped = Ty::list(Ty::Int).constrained("maxItems", 2);
        assert_eq!(
            errors_json(validate(&capped, &json!(["x", 1, 2, 3, 4])).unwrap_err()),
            r#"[{"type":"too_long","loc":[],"msg":"Tuple should have at most 2 items after validation, not 5","ctx":{"field_type":"Tuple","max_length":2,"actual_length":5}}]"#
        );
        // Two valid items never pass the cap, so only the bad one is reported.
        assert_eq!(validate(&capped, &json!([1, "x", 3])).unwrap_err().len(), 1);
        let floor = Ty::list(Ty::Str).constrained("minItems", 2);
        assert_eq!(
            errors_json(validate(&floor, &json!([1])).unwrap_err()),
            r#"[{"type":"string_type","loc":[0],"msg":"Input should be a valid string"},{"type":"too_short","loc":[],"msg":"Tuple should have at least 2 items after validation, not 0","ctx":{"field_type":"Tuple","min_length":2,"actual_length":0}}]"#
        );
    }

    #[test]
    fn bounds_are_reported_in_the_fields_own_type() {
        let unit = Ty::Float.ge(0).le(1);
        assert_eq!(
            errors_json(validate(&unit, &json!(2)).unwrap_err()),
            r#"[{"type":"less_than_equal","loc":[],"msg":"Input should be less than or equal to 1","ctx":{"le":1.0}}]"#
        );
        let positive = Ty::Int.gt(0).constrained("exclusiveMaximum", 10);
        assert_eq!(
            errors_json(validate(&positive, &json!(0)).unwrap_err()),
            r#"[{"type":"greater_than","loc":[],"msg":"Input should be greater than 0","ctx":{"gt":0}}]"#
        );
        assert_eq!(validate(&positive, &json!(10)).unwrap_err()[0]["type"], "less_than");
    }

    #[test]
    fn closed_sets_list_what_they_allow() {
        assert_eq!(
            errors_json(validate(&Ty::literals(&["lowest", "highest"]), &json!("cheapest")).unwrap_err()),
            r#"[{"type":"literal_error","loc":[],"msg":"Input should be 'lowest' or 'highest'","ctx":{"expected":"'lowest' or 'highest'"}}]"#
        );
        let ints = Ty::Literal(vec![json!(1), json!(2)]);
        assert_eq!(validate(&ints, &json!("1")).unwrap_err()[0]["msg"], "Input should be 1 or 2");
    }

    #[test]
    fn maps_and_nested_models_locate_their_errors() {
        let inner = ModelTy::frozen("I", vec![Field::required("x", Ty::Int)]).ty();
        let ty = Ty::map(inner.clone());
        assert_eq!(
            errors_json(validate(&ty, &json!({"a": {"x": "q"}, "b": 3})).unwrap_err()),
            r#"[{"type":"int_parsing","loc":["a","x"],"msg":"Input should be a valid integer, unable to parse string as an integer"},{"type":"model_type","loc":["b"],"msg":"Input should be an object","ctx":{"class_name":"I"}}]"#
        );
        assert_eq!(validate(&ty, &json!([])).unwrap_err()[0]["type"], "dict_type");
        let union = Ty::Union(vec![Ty::Int, Ty::Str]);
        assert_eq!(
            errors_json(validate(&union, &json!([1])).unwrap_err()),
            r#"[{"type":"int_type","loc":["int"],"msg":"Input should be a valid integer"},{"type":"string_type","loc":["str"],"msg":"Input should be a valid string"}]"#
        );
        assert_eq!(validate(&union, &json!("1")), Ok(json!("1")));
    }

    #[test]
    fn a_cut_literal_is_the_end_of_the_input() {
        for (text, reason) in [
            ("nul", "EOF while parsing a value at line 1 column 3"),
            ("not json", "expected ident at line 1 column 2"),
            ("", "EOF while parsing a value at line 1 column 0"),
            (r#"{"x": 1} trailing"#, "trailing characters at line 1 column 10"),
            (r#"{"a":1,}"#, "trailing comma at line 1 column 8"),
            (r#"{"a" 1}"#, "expected `:` at line 1 column 6"),
            ("{a:1}", "key must be a string at line 1 column 2"),
            (r#"{"a": "b\q"}"#, "invalid escape at line 1 column 10"),
        ] {
            let error = serde_json::from_str::<Value>(text).unwrap_err();
            assert_eq!(json_invalid(text, &error)["ctx"]["error"], reason, "{text}");
        }
    }
}
