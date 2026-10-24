//! Port of `src/fastbrowse/output_schema.py`: a JSON Schema as the model `run_task` takes for `output_schema`.
//!
//! `fastbrowse serve` gets its caller's output shape as JSON Schema draft 2020-12, since that is what a schema library
//! in another language can emit. Only keywords the model can enforce are taken. Any other is refused by name, so a
//! constraint the caller wrote is never dropped in silence.
//!
//! Python builds a pydantic model at run time. Here the same decisions build an [`OutputModel`]: a tree that validates
//! data as that model would, dumps it under the caller's names, and writes the JSON Schema pydantic would write for it.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Number, Value};

use crate::llm::ResponseSchema;
use crate::py::text;
use crate::schema::{self, Field as SchemaField, ModelTy, Ty};

pub const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// How a value stands to each bound. In this draft `exclusiveMinimum` holds the bound and is no flag on `minimum`.
/// Zod writes the two inclusive ones for every `z.int()`, so refusing them would refuse a plain integer.
const BOUNDS: [&str; 4] = ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"];
const ANYWHERE: [&str; 16] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "anyOf",
    "$ref",
    "description",
    "title",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
];
/// `title` and `default` are annotations in JSON Schema: no validator holds a value to either, so taking them drops
/// nothing the caller wrote. Zod writes them for `.meta({ title })` and `.default()`. They, and the root's `$schema`
/// and `$defs`, say nothing about the value and may sit beside any other keyword.
const NOTES: [&str; 5] = ["description", "title", "default", "$schema", "$defs"];
const OBJECT: [&str; 3] = ["properties", "required", "additionalProperties"];
const KNOWN_TYPES: [&str; 7] = ["object", "array", "string", "number", "integer", "boolean", "null"];
/// What a pydantic model already has, so a property of that name has to be given another field name.
const MODEL_ATTRIBUTES: [&str; 28] = [
    "construct",
    "copy",
    "dict",
    "from_orm",
    "json",
    "model_computed_fields",
    "model_config",
    "model_construct",
    "model_copy",
    "model_dump",
    "model_dump_json",
    "model_extra",
    "model_fields",
    "model_fields_set",
    "model_json_schema",
    "model_parametrized_name",
    "model_post_init",
    "model_rebuild",
    "model_validate",
    "model_validate_json",
    "model_validate_strings",
    "parse_file",
    "parse_obj",
    "parse_raw",
    "schema",
    "schema_json",
    "update_forward_refs",
    "validate",
];

/// A schema that cannot become a model, with the keyword at fault and a JSON pointer to it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{keyword}\" at {path}: {reason}")]
pub struct UnsupportedSchema {
    pub keyword: String,
    pub path: String,
    pub reason: String,
}

fn refuse<T>(keyword: &str, path: &str, reason: impl Into<String>) -> Result<T, UnsupportedSchema> {
    Err(UnsupportedSchema { keyword: keyword.into(), path: path.into(), reason: reason.into() })
}

/// Data the model would not take, with where and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{location}: {message}")]
pub struct ValidationError {
    pub location: String,
    pub message: String,
}

fn invalid<T>(location: &str, message: impl Into<String>) -> Result<T, ValidationError> {
    Err(ValidationError { location: location.into(), message: message.into() })
}

/// The type one schema became.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputType {
    /// `Annotated[str, Strict()]`.
    Str,
    /// A strict int that takes a float with no fractional part, held to its bounds.
    Int(Vec<(&'static str, f64)>),
    /// A strict float, held to its bounds.
    Number(Vec<(&'static str, f64)>),
    Bool,
    Null,
    Any,
    List(Box<OutputType>),
    Object(Arc<ObjectType>),
    /// `A | B | ...`, a `None` among them for a value that may be null.
    Union(Vec<OutputType>),
    /// `enum` or `const`: one of these values, as JSON compares them. `literal` when pydantic could write them as a
    /// `Literal` (strings, ints, bools and null), which is what the schema shows.
    OneOf {
        allowed: Vec<Value>,
        literal: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectType {
    /// What pydantic calls the model, and its `$defs` name.
    pub name: String,
    /// `"additionalProperties": false`.
    pub closed: bool,
    pub fields: Vec<OutputField>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputField {
    /// The property name, which data is read by and written under.
    pub key: String,
    /// The Python field name: the property name where Python allows it, and otherwise the nearest one it does. A run
    /// shows this name to the models when it asks for the value.
    pub name: String,
    pub required: bool,
    pub description: Option<String>,
    pub ty: OutputType,
}

/// The model for a JSON Schema whose root is an object.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputModel {
    pub root: Arc<ObjectType>,
}

/// `output_model(schema)`.
pub fn output_model(schema: &Value) -> Result<OutputModel, UnsupportedSchema> {
    Conversion::new(schema)?.model()
}

fn pointer(path: &str, token: &str) -> String {
    format!("{path}/{}", token.replace('~', "~0").replace('/', "~1"))
}

/// Whether a JSON value is of a JSON Schema type. `true` is no integer, and `3.0` is one.
fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0) && value.is_number(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        _ => value.is_object(),
    }
}

/// Equality as JSON has it, where `1` and `true` differ at any depth; `1` and `1.0` are equal, as in Python.
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => a.as_f64() == b.as_f64(),
        },
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
        }
        _ => left == right,
    }
}

/// Whether a property name can be the field's own name.
fn usable(name: &str) -> bool {
    text::is_identifier(name) && !text::is_keyword(name) && !name.starts_with('_') && !MODEL_ATTRIBUTES.contains(&name)
}

/// A field name for each property: its own where that works, and otherwise the nearest name that does.
fn field_names(properties: &Map<String, Value>) -> Vec<(String, String)> {
    let mut taken: std::collections::HashSet<String> = properties.keys().cloned().collect();
    properties
        .keys()
        .map(|name| {
            if usable(name) {
                return (name.clone(), name.clone());
            }
            let mut field: String = name.chars().map(|c| if text::is_word(c) { c } else { '_' }).collect();
            if !field.chars().next().is_some_and(char::is_alphabetic) {
                field = format!("field_{field}");
            }
            while taken.contains(&field) || !usable(&field) {
                field.push('_');
            }
            taken.insert(field.clone());
            (name.clone(), field)
        })
        .collect()
}

struct Conversion<'a> {
    schema: &'a Value,
    definitions: Map<String, Value>,
    resolved: HashMap<String, OutputType>,
    /// The definitions being read, outermost first. One that turns up again refers to itself.
    resolving: Vec<String>,
}

impl<'a> Conversion<'a> {
    fn new(schema: &'a Value) -> Result<Self, UnsupportedSchema> {
        let definitions = match schema.get("$defs") {
            None => Map::new(),
            Some(Value::Object(definitions)) => definitions.clone(),
            Some(_) if schema.is_object() => return refuse("$defs", "#/$defs", "has to be an object of schemas"),
            Some(_) => Map::new(),
        };
        Ok(Conversion { schema, definitions, resolved: HashMap::new(), resolving: Vec::new() })
    }

    fn model(mut self) -> Result<OutputModel, UnsupportedSchema> {
        let root = match self.annotation(self.schema, "#", "Output")? {
            OutputType::Object(root) => root,
            _ => {
                return refuse("type", "#/type", "a run returns an object, so the root has to be \"type\": \"object\"");
            }
        };
        // A definition nothing refers to is read as well, so an unsupported keyword in it is still refused.
        let names: Vec<String> = self.definitions.keys().cloned().collect();
        for name in names {
            self.definition(&name, &pointer("#/$defs", &name))?;
        }
        Ok(OutputModel { root })
    }

    /// The type for one schema. `name` is what a model built for it is called.
    fn annotation(&mut self, schema: &Value, path: &str, name: &str) -> Result<OutputType, UnsupportedSchema> {
        let Value::Object(map) = schema else {
            let last = path.rsplit_once('/').map_or(path, |(_, last)| last);
            return refuse(last, path, "has to be a schema object");
        };
        for key in map.keys() {
            let allowed =
                ANYWHERE.contains(&key.as_str()) || (path == "#" && matches!(key.as_str(), "$schema" | "$defs"));
            if !allowed {
                return refuse(key, &pointer(path, key), "this keyword is not supported");
            }
        }
        if path == "#" && map.get("$schema").is_some_and(|draft| draft != DRAFT) {
            return refuse("$schema", "#/$schema", format!("only {DRAFT} is supported"));
        }
        for note in ["description", "title"] {
            if map.get(note).is_some_and(|value| !value.is_string()) {
                return refuse(note, &pointer(path, note), "has to be a string");
            }
        }
        for alone in ["$ref", "anyOf", "enum", "const"] {
            if map.contains_key(alone) {
                Self::alone(map, path, alone)?;
            }
        }
        if let Some(reference) = map.get("$ref") {
            return self.reference(reference, &pointer(path, "$ref"));
        }
        if let Some(branches) = map.get("anyOf") {
            return self.nullable(branches, &pointer(path, "anyOf"), name);
        }
        let types = Self::types(map, path)?;
        if map.contains_key("enum") || map.contains_key("const") {
            return Self::closed(map, path, &types);
        }
        for key in OBJECT {
            if map.contains_key(key) && !types.contains(&"object") {
                return refuse(key, &pointer(path, key), "needs \"type\": \"object\" beside it");
            }
        }
        if map.contains_key("items") && !types.contains(&"array") {
            return refuse("items", &pointer(path, "items"), "needs \"type\": \"array\" beside it");
        }
        for key in BOUNDS {
            let Some(bound) = map.get(key) else { continue };
            if !types.iter().any(|t| matches!(*t, "integer" | "number")) {
                return refuse(key, &pointer(path, key), "needs \"type\": \"integer\" or \"number\" beside it");
            }
            if !has_type(bound, "number") {
                return refuse(key, &pointer(path, key), "has to be a number");
            }
        }
        if types.is_empty() {
            return Ok(OutputType::Any);
        }
        let mut members = Vec::new();
        for type_name in &types {
            members.push(self.typed(type_name, map, path, name)?);
        }
        Ok(union(members))
    }

    /// Refuse a keyword that would be ignored beside `key`, which decides the value on its own.
    fn alone(map: &Map<String, Value>, path: &str, key: &str) -> Result<(), UnsupportedSchema> {
        let closed = matches!(key, "enum" | "const");
        for other in map.keys() {
            let beside = NOTES.contains(&other.as_str())
                || other == key
                || (closed && matches!(other.as_str(), "type" | "enum" | "const"));
            if !beside {
                return refuse(other, &pointer(path, other), format!("is not supported beside \"{key}\""));
            }
        }
        Ok(())
    }

    fn types(map: &Map<String, Value>, path: &str) -> Result<Vec<&'static str>, UnsupportedSchema> {
        let Some(named) = map.get("type") else { return Ok(Vec::new()) };
        let listed: Vec<&Value> = match named {
            Value::Array(items) => items.iter().collect(),
            one => vec![one],
        };
        let known: Option<Vec<&'static str>> = listed
            .iter()
            .map(|one| one.as_str().and_then(|name| KNOWN_TYPES.iter().find(|known| **known == name).copied()))
            .collect();
        match known {
            Some(types) if !types.is_empty() => Ok(types),
            _ => refuse("type", &pointer(path, "type"), format!("has to be one or more of {}", KNOWN_TYPES.join(", "))),
        }
    }

    fn typed(
        &mut self,
        type_name: &str,
        map: &Map<String, Value>,
        path: &str,
        name: &str,
    ) -> Result<OutputType, UnsupportedSchema> {
        // A validator and not the field's own bound, which would be written into the schema the models are asked to
        // fill, and a provider's structured output may refuse a schema that has one.
        let bounds = || -> Vec<(&'static str, f64)> {
            BOUNDS.iter().filter_map(|key| map.get(*key).and_then(Value::as_f64).map(|b| (*key, b))).collect()
        };
        Ok(match type_name {
            "string" => OutputType::Str,
            "integer" => OutputType::Int(bounds()),
            "number" => OutputType::Number(bounds()),
            "boolean" => OutputType::Bool,
            "null" => OutputType::Null,
            "object" => self.object(map, path, name)?,
            _ => match map.get("items") {
                None => OutputType::List(Box::new(OutputType::Any)),
                Some(items) => {
                    let item = self.annotation(items, &pointer(path, "items"), &format!("{name}Item"))?;
                    OutputType::List(Box::new(item))
                }
            },
        })
    }

    fn object(&mut self, map: &Map<String, Value>, path: &str, name: &str) -> Result<OutputType, UnsupportedSchema> {
        let empty = Map::new();
        let properties = match map.get("properties") {
            None => &empty,
            Some(Value::Object(properties)) => properties,
            Some(_) => return refuse("properties", &pointer(path, "properties"), "has to be an object of schemas"),
        };
        let required: Vec<&str> = match map.get("required") {
            None => Vec::new(),
            Some(Value::Array(names))
                if names.iter().all(|n| n.as_str().is_some_and(|n| properties.contains_key(n))) =>
            {
                names.iter().filter_map(Value::as_str).collect()
            }
            // A required name with no schema could be anything, and a run has no way to fill it.
            Some(_) => {
                return refuse("required", &pointer(path, "required"), "has to be a list of names in properties");
            }
        };
        let closed = match map.get("additionalProperties") {
            None | Some(Value::Bool(true)) => false,
            // The empty schema takes any value, which is what `true` says. It is how Zod writes a loose object.
            Some(Value::Object(empty)) if empty.is_empty() => false,
            Some(Value::Bool(false)) => true,
            Some(_) => {
                return refuse(
                    "additionalProperties",
                    &pointer(path, "additionalProperties"),
                    "has to be true or false",
                );
            }
        };
        let mut fields = Vec::new();
        for ((key, field_name), property) in field_names(properties).into_iter().zip(properties.values()) {
            let at = pointer(&pointer(path, "properties"), &key);
            let ty = self.annotation(property, &at, &key)?;
            fields.push(OutputField {
                required: required.contains(&key.as_str()),
                description: property.get("description").and_then(Value::as_str).map(str::to_string),
                key,
                name: field_name,
                ty,
            });
        }
        Ok(OutputType::Object(Arc::new(ObjectType { name: name.to_string(), closed, fields })))
    }

    /// The values `enum` and `const` leave, of the types `type` leaves.
    fn closed(map: &Map<String, Value>, path: &str, types: &[&str]) -> Result<OutputType, UnsupportedSchema> {
        let key = if map.contains_key("enum") { "enum" } else { "const" };
        let mut allowed = match (map.get("enum"), map.get("const")) {
            (Some(Value::Array(values)), _) if !values.is_empty() => values.clone(),
            (Some(_), _) => return refuse("enum", &pointer(path, "enum"), "has to be a list with a value in it"),
            (None, Some(constant)) => vec![constant.clone()],
            (None, None) => unreachable!("closed is only asked for an enum or a const"),
        };
        if let (Some(_), Some(constant)) = (map.get("enum"), map.get("const")) {
            allowed.retain(|value| same(value, constant));
        }
        if !types.is_empty() {
            allowed.retain(|value| types.iter().any(|t| has_type(value, t)));
        }
        if allowed.is_empty() {
            return refuse(key, &pointer(path, key), "no value passes this together with what is beside it");
        }
        // `Literal` takes no float, list or object, so a set holding one is checked as `Any`.
        let literal =
            allowed.iter().all(|v| v.is_null() || v.is_string() || v.is_boolean() || v.is_i64() || v.is_u64());
        Ok(OutputType::OneOf { allowed, literal })
    }

    fn nullable(&mut self, branches: &Value, path: &str, name: &str) -> Result<OutputType, UnsupportedSchema> {
        if let Value::Array(items) = branches
            && items.len() == 2
        {
            for (index, branch) in items.iter().enumerate() {
                let is_null = branch.as_object().is_some_and(|b| {
                    b.get("type") == Some(&Value::from("null")) && b.keys().all(|k| k == "type" || k == "description")
                });
                if is_null {
                    let other = 1 - index;
                    let inner = self.annotation(&items[other], &pointer(path, &other.to_string()), name)?;
                    return Ok(union(vec![inner, OutputType::Null]));
                }
            }
        }
        refuse("anyOf", path, "only a schema and {\"type\": \"null\"} are supported, for a value that may be null")
    }

    fn reference(&mut self, reference: &Value, path: &str) -> Result<OutputType, UnsupportedSchema> {
        let name = reference
            .as_str()
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .filter(|name| !name.is_empty())
            .map(|name| name.replace("~1", "/").replace("~0", "~"));
        match name {
            Some(name) if self.definitions.contains_key(&name) => self.definition(&name, path),
            _ => refuse("$ref", path, "has to be #/$defs/ and the name of a definition in this schema"),
        }
    }

    fn definition(&mut self, name: &str, asked_at: &str) -> Result<OutputType, UnsupportedSchema> {
        if let Some(index) = self.resolving.iter().position(|n| n == name) {
            let mut cycle = self.resolving[index..].to_vec();
            cycle.push(name.to_string());
            return refuse("$ref", asked_at, format!("recursive schemas are not supported ({})", cycle.join(" -> ")));
        }
        if !self.resolved.contains_key(name) {
            self.resolving.push(name.to_string());
            let schema = self.definitions[name].clone();
            let ty = self.annotation(&schema, &pointer("#/$defs", name), name)?;
            self.resolving.pop();
            self.resolved.insert(name.to_string(), ty);
        }
        Ok(self.resolved[name].clone())
    }
}

/// `A | B`: one member stands alone, and a `None` among several goes last, as pydantic orders a nullable union.
fn union(members: Vec<OutputType>) -> OutputType {
    let mut flat = Vec::new();
    for member in members {
        match member {
            OutputType::Union(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    if flat.len() == 1 {
        return flat.remove(0);
    }
    if let Some(index) = flat.iter().position(|m| *m == OutputType::Null) {
        let null = flat.remove(index);
        flat.push(null);
    }
    OutputType::Union(flat)
}

impl OutputType {
    /// The schema pydantic writes for the type.
    pub fn ty(&self) -> Ty {
        match self {
            OutputType::Str => Ty::Str,
            OutputType::Int(_) => Ty::Int,
            OutputType::Number(_) => Ty::Float,
            OutputType::Bool => Ty::Bool,
            OutputType::Null => Ty::Null,
            OutputType::Any => Ty::Any,
            OutputType::List(item) => Ty::list(item.ty()),
            OutputType::Object(object) => object.ty(),
            OutputType::Union(members) => {
                let others: Vec<Ty> = members.iter().filter(|m| **m != OutputType::Null).map(OutputType::ty).collect();
                let nullable = others.len() < members.len();
                let inner =
                    if others.len() == 1 { others.into_iter().next().unwrap_or(Ty::Any) } else { Ty::Union(others) };
                if nullable { Ty::optional(inner) } else { inner }
            }
            OutputType::OneOf { allowed, literal: true } => Ty::Literal(allowed.clone()),
            OutputType::OneOf { literal: false, .. } => Ty::Any,
        }
    }

    fn validate(&self, value: &Value, at: &str) -> Result<Value, ValidationError> {
        match self {
            OutputType::Str if value.is_string() => Ok(value.clone()),
            OutputType::Str => invalid(at, "Input should be a valid string"),
            OutputType::Bool if value.is_boolean() => Ok(value.clone()),
            OutputType::Bool => invalid(at, "Input should be a valid boolean"),
            OutputType::Null if value.is_null() => Ok(Value::Null),
            OutputType::Null => invalid(at, "Input should be None"),
            OutputType::Any => Ok(value.clone()),
            OutputType::Int(bounds) => {
                // JSON has one number type, so JSON Schema calls 3.0 an integer, and a JavaScript client writes one
                // either way.
                let whole = match value {
                    Value::Number(n) if n.is_i64() || n.is_u64() => value.clone(),
                    Value::Number(n) => match n.as_f64() {
                        Some(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Value::from(f as i64),
                        _ => return invalid(at, "Input should be a valid integer"),
                    },
                    _ => return invalid(at, "Input should be a valid integer"),
                };
                within(bounds, whole.as_f64().unwrap_or(f64::NAN), at)?;
                Ok(whole)
            }
            OutputType::Number(bounds) => {
                let Some(number) = value.as_f64().filter(|_| value.is_number()) else {
                    return invalid(at, "Input should be a valid number");
                };
                within(bounds, number, at)?;
                Ok(Number::from_f64(number).map_or(Value::Null, Value::Number))
            }
            OutputType::List(item) => match value {
                Value::Array(items) => items
                    .iter()
                    .enumerate()
                    .map(|(index, element)| item.validate(element, &format!("{at}.{index}")))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array),
                _ => invalid(at, "Input should be a valid list"),
            },
            OutputType::Object(object) => object.validate(value, at),
            OutputType::Union(members) => {
                // pydantic's smart union: a member whose type the value has exactly wins over one that takes it.
                let exact = members.iter().find(|m| match m {
                    OutputType::Int(_) => value.is_i64() || value.is_u64(),
                    OutputType::Number(_) => value.is_f64(),
                    _ => false,
                });
                if let Some(Ok(valid)) = exact.map(|m| m.validate(value, at)) {
                    return Ok(valid);
                }
                let mut errors = Vec::new();
                for member in members {
                    match member.validate(value, at) {
                        Ok(valid) => return Ok(valid),
                        Err(error) => errors.push(error.message),
                    }
                }
                invalid(at, errors.join("; "))
            }
            OutputType::OneOf { allowed, literal } => match allowed.iter().find(|option| same(value, option)) {
                // A `Literal` hands back its own value; `Any` the value it was given.
                Some(option) if *literal => Ok(option.clone()),
                Some(_) => Ok(value.clone()),
                None => invalid(at, format!("not one of {}", python_list(allowed))),
            },
        }
    }
}

/// `repr(list)` of JSON values, for the message pydantic's validator raises.
fn python_list(values: &[Value]) -> String {
    fn repr(value: &Value) -> String {
        match value {
            Value::Null => "None".into(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Number(n) => match n.as_f64() {
                Some(f) if n.is_f64() => crate::py::json::float_str(f),
                _ => n.to_string(),
            },
            Value::String(s) => text::repr(s),
            Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
            Value::Object(map) => format!(
                "{{{}}}",
                map.iter().map(|(k, v)| format!("{}: {}", text::repr(k), repr(v))).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    repr(&Value::Array(values.to_vec()))
}

fn within(bounds: &[(&'static str, f64)], value: f64, at: &str) -> Result<(), ValidationError> {
    for (key, bound) in bounds {
        let holds = match *key {
            "minimum" => value >= *bound,
            "maximum" => value <= *bound,
            "exclusiveMinimum" => value > *bound,
            _ => value < *bound,
        };
        if !holds {
            return invalid(at, format!("outside {key} {}", crate::py::json::float_str(*bound)));
        }
    }
    Ok(())
}

impl ObjectType {
    pub fn ty(&self) -> Ty {
        let fields = self
            .fields
            .iter()
            .map(|field| SchemaField {
                name: field.name.clone(),
                alias: (field.name != field.key).then(|| field.key.clone()),
                ty: field.ty.ty(),
                default: (!field.required).then_some(Value::Null),
                description: field.description.clone(),
            })
            .collect();
        ModelTy { name: self.name.clone(), doc: None, forbid_extra: self.closed, fields }.ty()
    }

    /// `model_validate(data).model_dump(mode="json")`. A run fills the model by field name, a caller by property name;
    /// either is taken, and the caller's name is written. A property left out of `required` may be absent, which is
    /// not the same as null: the caller's own schema refuses a null where it allowed only a missing key, so an absent
    /// one stays absent.
    fn validate(&self, value: &Value, at: &str) -> Result<Value, ValidationError> {
        let Value::Object(data) = value else {
            return invalid(at, "Input should be a valid dictionary or instance of the model");
        };
        let mut dumped = Map::new();
        for field in &self.fields {
            let here = if at.is_empty() { field.key.clone() } else { format!("{at}.{}", field.key) };
            let given = data.get(&field.key).or_else(|| data.get(&field.name));
            match given {
                Some(item) => {
                    dumped.insert(field.key.clone(), field.ty.validate(item, &here)?);
                }
                None if field.required => return invalid(&here, "Field required"),
                None => {}
            }
        }
        if self.closed {
            let named = |key: &String| self.fields.iter().any(|f| &f.key == key || &f.name == key);
            if let Some(extra) = data.keys().find(|key| !named(key)) {
                let here = if at.is_empty() { extra.clone() } else { format!("{at}.{extra}") };
                return invalid(&here, "Extra inputs are not permitted");
            }
        }
        Ok(Value::Object(dumped))
    }
}

impl OutputModel {
    pub fn fields(&self) -> &[OutputField] {
        &self.root.fields
    }

    pub fn validate(&self, data: &Value) -> Result<Value, ValidationError> {
        self.root.validate(data, "")
    }

    /// `model_json_schema()`.
    pub fn json_schema(&self) -> Value {
        schema::json_schema(&self.root.ty())
    }
}

impl ResponseSchema for OutputModel {
    fn name(&self) -> String {
        self.root.name.clone()
    }

    fn json_schema(&self) -> Value {
        OutputModel::json_schema(self)
    }

    fn validate_json(&self, text: &str) -> Result<Value, String> {
        let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        self.validate(&data).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    //! tests/test_output_schema.py, apart from the extraction case, which belongs to verification.
    use super::*;
    use serde_json::json;

    fn order() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "customer": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}, "vip": {"type": "boolean"}},
                    "required": ["name", "vip"],
                },
                "lines": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"sku": {"type": "string"}, "quantity": {"type": "integer"}},
                        "required": ["sku", "quantity"],
                    },
                },
            },
            "required": ["id", "customer", "lines"],
        })
    }

    fn holding(schema: Value) -> Value {
        json!({"type": "object", "properties": {"value": schema}, "required": ["value"]})
    }

    fn accepts(schema: &Value, value: &Value) -> bool {
        output_model(&holding(schema.clone())).unwrap().validate(&json!({ "value": value })).is_ok()
    }

    fn dumped(model: &OutputModel, data: Value) -> Value {
        model.validate(&data).unwrap()
    }

    #[test]
    fn test_nested_objects_and_arrays_validate_matching_data() {
        let order_data = json!({
            "id": "A-17",
            "customer": {"name": "Ada", "vip": true},
            "lines": [{"sku": "kettle", "quantity": 2}, {"sku": "mug", "quantity": 6}],
        });
        assert_eq!(dumped(&output_model(&order()).unwrap(), order_data.clone()), order_data);
    }

    #[test]
    fn test_nested_objects_and_arrays_reject_mismatched_data() {
        let model = output_model(&order()).unwrap();
        for mismatched in [
            json!({"id": "A-17", "customer": {"name": "Ada"}, "lines": []}),
            json!({"id": "A-17", "customer": {"name": "Ada", "vip": true}, "lines": [{"sku": "kettle", "quantity": "two"}]}),
            json!({"id": "A-17", "customer": "Ada", "lines": []}),
            json!({"id": "A-17", "customer": {"name": "Ada", "vip": true}, "lines": {"sku": "kettle", "quantity": 2}}),
            json!({"customer": {"name": "Ada", "vip": true}, "lines": []}),
        ] {
            assert!(model.validate(&mismatched).is_err(), "{mismatched}");
        }
    }

    #[test]
    fn test_each_supported_keyword_is_enforced() {
        let sku = json!({"sku": {"type": "string"}});
        let cases: Vec<(Value, Vec<Value>, Vec<Value>)> = vec![
            (
                json!({"type": "string"}),
                vec![json!("kettle"), json!("")],
                vec![json!(3), json!(null), json!(["kettle"])],
            ),
            (
                json!({"type": "integer"}),
                vec![json!(3), json!(-1), json!(3.0), json!(-0.0), json!(1e3)],
                vec![json!("three"), json!("3"), json!("3.0"), json!(3.5), json!(true), json!(null)],
            ),
            (
                json!({"type": "number"}),
                vec![json!(3.5), json!(3)],
                vec![json!("three"), json!("3.5"), json!(true), json!(null)],
            ),
            (json!({"type": "boolean"}), vec![json!(true), json!(false)], vec![json!("yes"), json!(1), json!(null)]),
            (json!({"type": "null"}), vec![json!(null)], vec![json!("null"), json!(0)]),
            (json!({"type": ["string", "null"]}), vec![json!("kettle"), json!(null)], vec![json!(3.5)]),
            (
                json!({"type": "array", "items": {"type": "integer"}}),
                vec![json!([]), json!([1, 2])],
                vec![json!(["one"]), json!(1), json!({"0": 1})],
            ),
            (json!({"type": "array"}), vec![json!([]), json!([1, "two", null])], vec![json!(1), json!("one")]),
            (
                json!({"type": "object", "properties": sku}),
                vec![json!({"sku": "kettle"}), json!({})],
                vec![json!({"sku": 3}), json!("kettle")],
            ),
            (
                json!({"type": "object", "properties": sku, "required": ["sku"]}),
                vec![json!({"sku": "kettle"})],
                vec![json!({}), json!({"sku": null})],
            ),
            (
                json!({"type": "object", "properties": sku, "additionalProperties": false}),
                vec![json!({"sku": "kettle"})],
                vec![json!({"sku": "kettle", "price": 30})],
            ),
            (
                json!({"type": "object", "properties": sku, "additionalProperties": true}),
                vec![json!({"sku": "kettle", "price": 30})],
                vec![json!({"sku": 3})],
            ),
            (
                json!({"enum": ["small", "large", 3, null]}),
                vec![json!("small"), json!(3), json!(null)],
                vec![json!("medium"), json!(4)],
            ),
            (
                json!({"enum": [1.5, [1, 2], {"size": "small"}]}),
                vec![json!(1.5), json!([1, 2]), json!({"size": "small"})],
                vec![json!(2.5), json!([2, 1]), json!({})],
            ),
            (
                json!({"type": "string", "enum": ["small", "large"]}),
                vec![json!("large")],
                vec![json!("medium"), json!(null)],
            ),
            (json!({"type": "integer", "enum": [1, "one"]}), vec![json!(1)], vec![json!("one"), json!(2)]),
            (json!({"const": "kettle"}), vec![json!("kettle")], vec![json!("mug"), json!(null)]),
            (json!({"type": "integer", "const": 3}), vec![json!(3)], vec![json!(4)]),
            (json!({"const": 1}), vec![json!(1)], vec![json!(true), json!("1")]),
            (json!({"enum": [true]}), vec![json!(true)], vec![json!(1)]),
            (
                json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
                vec![json!("kettle"), json!(null)],
                vec![json!(3.5)],
            ),
            (
                json!({"anyOf": [{"type": "null"}, {"type": "array", "items": {"type": "string"}}]}),
                vec![json!(["a"]), json!(null)],
                vec![json!([1]), json!("a")],
            ),
            (
                json!({"type": "string", "description": "What the item is called"}),
                vec![json!("kettle")],
                vec![json!(3)],
            ),
            (json!({"type": "string", "title": "Name"}), vec![json!("kettle")], vec![json!(3)]),
            (json!({"type": "string", "default": "kettle"}), vec![json!("mug")], vec![json!(3)]),
            (
                json!({"type": "object", "properties": sku, "additionalProperties": {}}),
                vec![json!({"sku": "kettle", "price": 30})],
                vec![json!({"sku": 3})],
            ),
            (
                json!({"type": "integer", "minimum": 0}),
                vec![json!(0), json!(7)],
                vec![json!(-1), json!(0.5), json!("0")],
            ),
            (
                json!({"type": "integer", "maximum": 9007199254740991_i64}),
                vec![json!(9007199254740991_i64)],
                vec![json!(9007199254740992_i64)],
            ),
            (json!({"type": "number", "exclusiveMinimum": 0}), vec![json!(0.5), json!(1)], vec![json!(0), json!(-2)]),
            (
                json!({"type": "number", "exclusiveMaximum": 1.5, "minimum": 1}),
                vec![json!(1), json!(1.25)],
                vec![json!(1.5), json!(0.5)],
            ),
            (json!({"type": ["integer", "null"], "minimum": 0}), vec![json!(3), json!(null)], vec![json!(-3)]),
            (json!({}), vec![json!("kettle"), json!(3), json!(null), json!([1]), json!({"a": 1})], vec![]),
        ];
        for (schema, matching, mismatched) in cases {
            for value in &matching {
                assert!(accepts(&schema, value), "{schema} should take {value}");
            }
            for value in &mismatched {
                assert!(!accepts(&schema, value), "{schema} should refuse {value}");
            }
        }
    }

    #[test]
    fn test_what_zod_writes_for_a_typical_schema_is_taken() {
        let model = output_model(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The product name"},
                "count": {"type": "integer", "minimum": -9007199254740991_i64, "maximum": 9007199254740991_i64},
                "rating": {"type": ["number", "null"]},
                "size": {"type": "string", "enum": ["small", "large"]},
                "currency": {"default": "EUR", "type": "string"},
                "seller": {"$ref": "#/$defs/Seller"},
            },
            "required": ["name", "count", "rating", "size", "seller"],
            "title": "Product",
            "$defs": {"Seller": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
        }))
        .unwrap();
        let product = json!({"name": "Kettle", "count": 2, "rating": null, "size": "small", "seller": {"name": "Ada"}});
        assert_eq!(dumped(&model, product.clone()), product);
        let mut too_big = product;
        too_big["count"] = json!(9007199254740992_i64);
        assert!(model.validate(&too_big).is_err());
    }

    #[test]
    fn test_a_bound_is_not_part_of_the_schema_a_model_is_shown() {
        let model = output_model(&holding(json!({"type": "integer", "minimum": 0}))).unwrap();
        assert_eq!(model.json_schema()["properties"]["value"], json!({"title": "Value", "type": "integer"}));
    }

    #[test]
    fn test_a_description_reaches_the_field_a_run_reads_it_from() {
        let model = output_model(&holding(json!({"type": "string", "description": "The order number"}))).unwrap();
        assert_eq!(model.fields()[0].description.as_deref(), Some("The order number"));
    }

    #[test]
    fn test_a_property_left_out_of_required_may_be_absent_and_comes_back_absent() {
        let model = output_model(
            &json!({"type": "object", "properties": {"sku": {"type": "string"}, "note": {"type": "string"}}}),
        )
        .unwrap();
        assert_eq!(dumped(&model, json!({"sku": "kettle"})), json!({"sku": "kettle"}));
    }

    #[test]
    fn test_a_null_the_schema_allows_comes_back_as_null() {
        let model = output_model(
            &json!({"type": "object", "properties": {"note": {"anyOf": [{"type": "string"}, {"type": "null"}]}}}),
        )
        .unwrap();
        assert_eq!(dumped(&model, json!({"note": null})), json!({"note": null}));
        assert_eq!(dumped(&model, json!({})), json!({}));
    }

    #[test]
    fn test_definitions_are_followed_through_ref() {
        let model = output_model(&json!({
            "type": "object",
            "properties": {
                "billing": {"$ref": "#/$defs/address"},
                "shipping": {"$ref": "#/$defs/address", "description": "Where it goes"},
                "history": {"type": "array", "items": {"$ref": "#/$defs/address"}},
            },
            "required": ["billing", "shipping", "history"],
            "$defs": {
                "address": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}, "country": {"$ref": "#/$defs/country"}},
                    "required": ["city", "country"],
                },
                "country": {"enum": ["nl", "de"]},
            },
        }))
        .unwrap();
        let home = json!({"city": "Utrecht", "country": "nl"});
        let order = json!({"billing": home, "shipping": home, "history": [home]});
        assert_eq!(dumped(&model, order.clone()), order);
        let mut abroad = order;
        abroad["shipping"] = json!({"city": "Paris", "country": "fr"});
        assert!(model.validate(&abroad).is_err());
    }

    fn refusal(schema: Value) -> UnsupportedSchema {
        output_model(&schema).unwrap_err()
    }

    #[test]
    fn test_a_keyword_that_cannot_be_honored_is_refused_by_name_and_path() {
        let sku = json!({"sku": {"type": "string"}});
        for (schema, keyword, at) in [
            (json!({"type": "string", "minLength": 1}), "minLength", "/minLength"),
            (json!({"type": "string", "pattern": "^A-"}), "pattern", "/pattern"),
            (json!({"type": "string", "format": "date"}), "format", "/format"),
            (json!({"type": "number", "multipleOf": 0.5}), "multipleOf", "/multipleOf"),
            (json!({"type": "string", "minimum": 0}), "minimum", "/minimum"),
            (json!({"minimum": 0}), "minimum", "/minimum"),
            (json!({"type": "integer", "minimum": "0"}), "minimum", "/minimum"),
            (json!({"type": "integer", "maximum": true}), "maximum", "/maximum"),
            (json!({"enum": [1, 2], "maximum": 1}), "maximum", "/maximum"),
            (json!({"type": "string", "title": 3}), "title", "/title"),
            (json!({"type": "array", "items": {"type": "string"}, "minItems": 1}), "minItems", "/minItems"),
            (json!({"type": "array", "items": {"type": "string", "maxLength": 3}}), "maxLength", "/items/maxLength"),
            (json!({"type": "array", "prefixItems": [{"type": "string"}]}), "prefixItems", "/prefixItems"),
            (json!({"type": "array", "uniqueItems": true}), "uniqueItems", "/uniqueItems"),
            (json!({"oneOf": [{"type": "string"}, {"type": "integer"}]}), "oneOf", "/oneOf"),
            (json!({"allOf": [{"type": "string"}]}), "allOf", "/allOf"),
            (json!({"not": {"type": "string"}}), "not", "/not"),
            (json!({"type": "object", "patternProperties": {"^x-": {}}}), "patternProperties", "/patternProperties"),
            (
                json!({"type": "object", "properties": {"a/b": {"type": "string", "maxLength": 3}}}),
                "maxLength",
                "/properties/a~1b/maxLength",
            ),
            (json!({"anyOf": [{"type": "string"}, {"type": "integer"}]}), "anyOf", "/anyOf"),
            (
                json!({"anyOf": [{"type": "string", "minLength": 1}, {"type": "null"}]}),
                "minLength",
                "/anyOf/0/minLength",
            ),
            (json!({"anyOf": [{"type": "string"}, {"type": "null"}], "type": "string"}), "type", "/type"),
            (
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
                "additionalProperties",
                "/additionalProperties",
            ),
            (json!({"type": "object", "required": ["sku"]}), "required", "/required"),
            (json!({"type": "string", "properties": sku}), "properties", "/properties"),
            (json!({"type": "string", "items": {}}), "items", "/items"),
            (json!({"type": "date"}), "type", "/type"),
            (json!({"type": "string", "enum": [1, 2]}), "enum", "/enum"),
            (json!({"enum": []}), "enum", "/enum"),
            (json!({"enum": ["a"], "items": {}}), "items", "/items"),
            (json!({"$ref": "#/$defs/missing"}), "$ref", "/$ref"),
            (json!({"$ref": "https://example.com/address.json"}), "$ref", "/$ref"),
            (json!({"$ref": "#/properties/value"}), "$ref", "/$ref"),
            (json!({"$ref": "#"}), "$ref", "/$ref"),
            (json!({"$ref": "#/$defs/missing", "type": "string"}), "type", "/type"),
            (json!({"type": "object", "$defs": {"a": {"type": "string"}}}), "$defs", "/$defs"),
            (json!({"type": "array", "items": true}), "items", "/items"),
        ] {
            let refused = refusal(holding(schema.clone()));
            let path = format!("#/properties/value{at}");
            assert_eq!((refused.keyword.as_str(), refused.path.as_str()), (keyword, path.as_str()), "{schema}");
            assert!(refused.to_string().contains(&format!("\"{keyword}\"")) && refused.to_string().contains(&path));
        }
    }

    #[test]
    fn test_a_root_that_cannot_be_a_runs_output_is_refused() {
        for (schema, keyword, path) in [
            (json!({"type": "array", "items": {"type": "string"}}), "type", "#/type"),
            (json!({"type": "string"}), "type", "#/type"),
            (json!({}), "type", "#/type"),
            (json!({"type": "object", "$schema": "http://json-schema.org/draft-07/schema#"}), "$schema", "#/$schema"),
            (json!({"type": "object", "$id": "https://example.com/order"}), "$id", "#/$id"),
            (
                json!({"type": "object", "$defs": {"unused": {"type": "string", "minLength": 1}}}),
                "minLength",
                "#/$defs/unused/minLength",
            ),
        ] {
            let refused = refusal(schema.clone());
            assert_eq!((refused.keyword.as_str(), refused.path.as_str()), (keyword, path), "{schema}");
        }
    }

    #[test]
    fn test_a_recursive_ref_is_refused() {
        for (definitions, path) in [
            (
                json!({"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}}),
                "#/$defs/node/properties/next/$ref",
            ),
            (
                json!({
                    "node": {"type": "object", "properties": {"children": {"$ref": "#/$defs/nodes"}}},
                    "nodes": {"type": "array", "items": {"$ref": "#/$defs/node"}},
                }),
                "#/$defs/nodes/items/$ref",
            ),
            (json!({"node": {"anyOf": [{"$ref": "#/$defs/node"}, {"type": "null"}]}}), "#/$defs/node/anyOf/0/$ref"),
        ] {
            let refused = refusal(
                json!({"type": "object", "properties": {"tree": {"$ref": "#/$defs/node"}}, "$defs": definitions}),
            );
            assert_eq!((refused.keyword.as_str(), refused.path.as_str()), ("$ref", path));
            assert!(refused.to_string().contains("recursive"));
        }
    }

    const AWKWARD: [&str; 11] =
        ["order-id", "2fa", "class", "copy", "model_dump", "_private", "", "total price", "na\u{ef}ve", "a/b", "~"];

    #[test]
    fn test_a_property_python_could_not_name_round_trips_under_the_callers_name() {
        for name in AWKWARD {
            let model =
                output_model(&json!({"type": "object", "properties": {name: {"type": "string"}}, "required": [name]}))
                    .unwrap();
            assert_eq!(dumped(&model, json!({ name: "kept" })), json!({ name: "kept" }), "{name:?}");
        }
    }

    #[test]
    fn test_awkward_names_round_trip_together_and_at_every_depth() {
        let names: Map<String, Value> = AWKWARD.iter().map(|n| (n.to_string(), json!({"type": "string"}))).collect();
        let inner = json!({"type": "object", "properties": names, "required": AWKWARD});
        // `order_id` is what `order-id` would otherwise be called, and both have to survive.
        let mut properties = names.clone();
        properties.insert("order_id".into(), json!({"type": "string"}));
        properties.insert("in".into(), inner.clone());
        properties.insert("for".into(), json!({"type": "array", "items": inner}));
        let required: Vec<String> = properties.keys().cloned().collect();
        let model = output_model(&json!({"type": "object", "properties": properties, "required": required})).unwrap();
        let flat: Map<String, Value> =
            AWKWARD.iter().map(|n| (n.to_string(), json!(format!("value of {n}")))).collect();
        let mut data = flat.clone();
        data.insert("order_id".into(), json!("underscored"));
        data.insert("in".into(), Value::Object(flat.clone()));
        data.insert("for".into(), json!([flat, flat]));
        assert_eq!(dumped(&model, Value::Object(data.clone())), Value::Object(data));
    }

    #[test]
    fn test_a_run_that_fills_the_model_by_field_name_gets_the_callers_names_back() {
        let model = output_model(&json!({
            "type": "object",
            "properties": {"order-id": {"type": "string"}, "class": {"type": "integer"}, "total": {"type": "number"}},
            "required": ["order-id", "class", "total"],
        }))
        .unwrap();
        let name = |key: &str| model.fields().iter().find(|f| f.key == key).unwrap().name.clone();
        assert_eq!(name("order-id"), "order_id");
        assert_eq!(name("class"), "class_");
        // `verification.extract` keys what it found by field name and dumps with no arguments.
        let filled = json!({ name("order-id"): "A-17", name("class"): 2, name("total"): 9.5 });
        assert_eq!(dumped(&model, filled), json!({"order-id": "A-17", "class": 2, "total": 9.5}));
    }
}
