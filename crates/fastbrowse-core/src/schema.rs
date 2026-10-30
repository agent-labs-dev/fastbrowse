//! JSON Schema as pydantic's `model_json_schema()` writes it, and the strict form structured output needs.
//!
//! A structured-output request carries the response model's schema, so a Rust request must carry the same bytes as
//! Python's or a recorded reply no longer answers it. Types describe themselves as a [`Ty`] tree, the shape pydantic's
//! core schema has, and [`json_schema`] writes it the way `GenerateJsonSchema` does: models and enums under `$defs`,
//! a title for every field that is not a reference, every key sorted except property names. The models
//! `output_schema` builds at run time are the same tree.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::py::text;

/// A type as the schema sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Str,
    Int,
    Float,
    Bool,
    Null,
    /// `Any`: the empty schema.
    Any,
    /// pydantic's `JsonValue`, a named alias with an empty schema.
    JsonValue,
    DateTime,
    /// `bytes`.
    Bytes,
    /// `Base64Bytes`: bytes read and written as base64.
    Base64,
    /// `Image`: bytes read as `bytes`, written as a base64 string.
    Image,
    /// A `pathlib.Path`.
    Path,
    /// `tuple[T, ...]` or `list[T]`.
    List(Box<Ty>),
    /// `tuple[A, B]`.
    Tuple(Vec<Ty>),
    /// `frozenset[T]`.
    Set(Box<Ty>),
    /// `dict[str, T]` or `Mapping[str, T]`.
    Map(Box<Ty>),
    /// `T | None`.
    Optional(Box<Ty>),
    /// `A | B | ...`.
    Union(Vec<Ty>),
    /// `Literal[...]`.
    Literal(Vec<Value>),
    Enum(Arc<EnumTy>),
    Model(Arc<ModelTy>),
    /// A PEP 695 `type` alias, which pydantic puts under `$defs` by its name.
    Alias(&'static str, Box<Ty>),
    /// A union told apart by one property's literal value.
    Tagged {
        property: &'static str,
        variants: Vec<(&'static str, Ty)>,
    },
    /// Keywords pydantic writes beside the type: `minimum`, `maxItems` and the like, from `Field(ge=...)`.
    Constrained(Box<Ty>, Vec<(&'static str, Value)>),
}

impl Ty {
    pub fn list(item: Ty) -> Ty {
        Ty::List(Box::new(item))
    }
    pub fn map(value: Ty) -> Ty {
        Ty::Map(Box::new(value))
    }
    pub fn optional(inner: Ty) -> Ty {
        Ty::Optional(Box::new(inner))
    }
    pub fn literal(value: &str) -> Ty {
        Ty::Literal(vec![Value::from(value)])
    }
    pub fn literals(values: &[&str]) -> Ty {
        Ty::Literal(values.iter().map(|v| Value::from(*v)).collect())
    }
    pub fn constrained(self, keyword: &'static str, bound: impl Into<Value>) -> Ty {
        match self {
            Ty::Constrained(inner, mut keywords) => {
                keywords.push((keyword, bound.into()));
                Ty::Constrained(inner, keywords)
            }
            other => Ty::Constrained(Box::new(other), vec![(keyword, bound.into())]),
        }
    }
    pub fn ge(self, bound: impl Into<Value>) -> Ty {
        self.constrained("minimum", bound)
    }
    pub fn gt(self, bound: impl Into<Value>) -> Ty {
        self.constrained("exclusiveMinimum", bound)
    }
    pub fn le(self, bound: impl Into<Value>) -> Ty {
        self.constrained("maximum", bound)
    }

    /// Whether a field of this type gets a title. pydantic leaves it off a field whose schema is a reference to a
    /// model or an enum, looking through `None` and a default to find one.
    fn titled(&self) -> bool {
        match self {
            Ty::Model(_) | Ty::Enum(_) | Ty::Tagged { .. } | Ty::Alias(..) | Ty::JsonValue => false,
            Ty::Optional(inner) => inner.titled(),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumTy {
    pub name: &'static str,
    pub doc: Option<&'static str>,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelTy {
    pub name: String,
    /// The Python module that defines it, which pydantic writes into a `$defs` name two models would share.
    pub module: &'static str,
    pub doc: Option<String>,
    /// `extra="forbid"`, written as `"additionalProperties": false`.
    pub forbid_extra: bool,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The Python attribute name.
    pub name: String,
    /// The name in JSON, when it is not `name`.
    pub alias: Option<String>,
    pub ty: Ty,
    /// The default as JSON; `None` for a required field.
    pub default: Option<Value>,
    pub description: Option<String>,
    /// `default_factory`: not required, and pydantic writes no default for it.
    pub factory: bool,
    /// `exclude=True`: never written, so a serialization schema leaves it out.
    pub exclude: bool,
}

impl Field {
    pub fn required(name: &str, ty: Ty) -> Field {
        Field { name: name.into(), alias: None, ty, default: None, description: None, factory: false, exclude: false }
    }
    pub fn default(name: &str, ty: Ty, default: impl Into<Value>) -> Field {
        Field { default: Some(default.into()), ..Field::required(name, ty) }
    }
    /// A field with a `default_factory`, which the schema leaves out of `required` and writes no default for.
    pub fn factory(name: &str, ty: Ty) -> Field {
        Field { factory: true, ..Field::required(name, ty) }
    }
    /// A field the model holds and never writes.
    pub fn excluded(self) -> Field {
        Field { exclude: true, ..self }
    }
    pub fn doc(mut self, description: &str) -> Field {
        self.description = Some(description.into());
        self
    }
    pub fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
    pub fn is_required(&self) -> bool {
        self.default.is_none() && !self.factory
    }
}

impl ModelTy {
    /// A `Frozen` model: `extra="forbid"`, field docstrings as descriptions.
    pub fn frozen(name: &str, fields: Vec<Field>) -> ModelTy {
        ModelTy { name: name.into(), module: "fastbrowse.models", doc: None, forbid_extra: true, fields }
    }
    pub fn doc(mut self, doc: &str) -> ModelTy {
        self.doc = Some(doc.into());
        self
    }
    pub fn ty(self) -> Ty {
        Ty::Model(Arc::new(self))
    }
}

/// A type that can describe itself for its JSON Schema.
pub trait Schema {
    fn ty() -> Ty;
}

macro_rules! scalar_schema {
    ($($rust:ty => $ty:expr),* $(,)?) => {
        $(impl Schema for $rust { fn ty() -> Ty { $ty } })*
    };
}

scalar_schema!(String => Ty::Str, bool => Ty::Bool, i64 => Ty::Int, u64 => Ty::Int, f64 => Ty::Float, Value => Ty::JsonValue);

impl<T: Schema> Schema for Option<T> {
    fn ty() -> Ty {
        Ty::optional(T::ty())
    }
}

impl<T: Schema> Schema for Vec<T> {
    fn ty() -> Ty {
        Ty::list(T::ty())
    }
}

/// `model_json_schema()` of a model or any other type.
pub fn json_schema(ty: &Ty) -> Value {
    let mut generator = Generator::default();
    generator.collect(ty);
    let root = match ty {
        Ty::Model(model) => generator.model(model),
        other => generator.schema(other),
    };
    let mut root = match root {
        Value::Object(map) => map,
        other => return other,
    };
    if !generator.defs.is_empty() {
        let defs: Map<String, Value> = generator.defs.into_iter().collect();
        root.insert("$defs".into(), Value::Object(defs));
    }
    sort(Value::Object(root), None)
}

/// Which way a model travels, as pydantic's `JsonSchemaMode` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// What is read: a field with a default may be left out.
    Validation,
    /// What is written: every field is there, a default included, and an excluded one never is.
    Serialization,
}

/// `models_json_schema()` of several types at once: one document whose `$defs` hold each of them and what they refer
/// to, every one under its own name. A field with a default is required in a type written in [`Mode::Serialization`],
/// since the writer always puts it there.
pub fn definitions(roots: &[(Ty, Mode)]) -> Value {
    let mut generator = Generator::default();
    for (ty, _) in roots {
        generator.collect(ty);
    }
    for (ty, mode) in roots {
        generator.mode = *mode;
        generator.schema(ty);
    }
    let defs: Map<String, Value> = generator.defs.into_iter().collect();
    sort(json!({ "$defs": defs }), None)
}

/// `json_schema` of a type that describes itself.
pub fn json_schema_of<T: Schema>() -> Value {
    json_schema(&T::ty())
}

struct Generator {
    mode: Mode,
    defs: BTreeMap<String, Value>,
    /// Each distinct model under a name, in the order they are met.
    models: BTreeMap<String, Vec<Arc<ModelTy>>>,
}

impl Default for Generator {
    fn default() -> Self {
        Generator { mode: Mode::Validation, defs: BTreeMap::new(), models: BTreeMap::new() }
    }
}

impl Generator {
    /// Every model the type holds, so two different models sharing a name can be told apart.
    fn collect(&mut self, ty: &Ty) {
        match ty {
            Ty::Model(model) => {
                let seen = self.models.entry(model.name.clone()).or_default();
                if seen.iter().any(|other| other == model) {
                    return;
                }
                seen.push(model.clone());
                for field in &model.fields {
                    self.collect(&field.ty);
                }
            }
            Ty::List(inner) | Ty::Set(inner) | Ty::Map(inner) | Ty::Optional(inner) | Ty::Alias(_, inner) => {
                self.collect(inner)
            }
            Ty::Constrained(inner, _) => self.collect(inner),
            Ty::Tuple(items) | Ty::Union(items) => items.iter().for_each(|item| self.collect(item)),
            Ty::Tagged { variants, .. } => variants.iter().for_each(|(_, variant)| self.collect(variant)),
            _ => {}
        }
    }

    /// A model's `$defs` name: its own, unless another model has it too. Then pydantic writes each as its module and
    /// name with a count, `fastbrowse__output_schema__item__1`, as an output schema with two `item` objects gets.
    fn def_name(&self, model: &ModelTy) -> String {
        match self.models.get(&model.name) {
            Some(models) if models.len() > 1 => {
                let index = models.iter().position(|other| **other == *model).unwrap_or(0) + 1;
                format!("{}__{}__{index}", model.module.replace('.', "__"), model.name)
            }
            _ => model.name.clone(),
        }
    }

    fn reference(&mut self, name: &str, build: impl FnOnce(&mut Self) -> Value) -> Value {
        if !self.defs.contains_key(name) {
            // Held before it is built, so a model that refers to itself finds its own name.
            self.defs.insert(name.to_string(), Value::Null);
            let schema = build(self);
            self.defs.insert(name.to_string(), schema);
        }
        json!({ "$ref": format!("#/$defs/{name}") })
    }

    fn schema(&mut self, ty: &Ty) -> Value {
        match ty {
            Ty::Str => json!({"type": "string"}),
            Ty::Int => json!({"type": "integer"}),
            Ty::Float => json!({"type": "number"}),
            Ty::Bool => json!({"type": "boolean"}),
            Ty::Null => json!({"type": "null"}),
            Ty::Any => json!({}),
            Ty::JsonValue => self.reference("JsonValue", |_| json!({})),
            Ty::DateTime => json!({"format": "date-time", "type": "string"}),
            Ty::Bytes => json!({"format": "binary", "type": "string"}),
            Ty::Base64 => json!({"format": "base64", "type": "string"}),
            Ty::Image => match self.mode {
                Mode::Validation => json!({"format": "binary", "type": "string"}),
                Mode::Serialization => json!({"type": "string"}),
            },
            Ty::Path => json!({"format": "path", "type": "string"}),
            Ty::List(item) => json!({"items": self.schema(item), "type": "array"}),
            Ty::Set(item) => json!({"items": self.schema(item), "type": "array", "uniqueItems": true}),
            Ty::Tuple(items) => {
                let items: Vec<Value> = items.iter().map(|item| self.schema(item)).collect();
                let count = items.len();
                json!({"maxItems": count, "minItems": count, "prefixItems": items, "type": "array"})
            }
            // pydantic writes `dict[str, Any]` as any object at all.
            Ty::Map(value) if **value == Ty::Any => json!({"additionalProperties": true, "type": "object"}),
            Ty::Map(value) => json!({"additionalProperties": self.schema(value), "type": "object"}),
            Ty::Optional(inner) => {
                let mut branches = self.branches(std::slice::from_ref(&**inner));
                branches.push(json!({"type": "null"}));
                json!({ "anyOf": branches })
            }
            Ty::Union(options) => json!({ "anyOf": self.branches(options) }),
            Ty::Literal(values) => literal(values),
            Ty::Enum(definition) => {
                let definition = definition.clone();
                self.reference(definition.name, |_| {
                    let mut schema = Map::new();
                    if let Some(doc) = definition.doc {
                        schema.insert("description".into(), doc.into());
                    }
                    schema.insert("enum".into(), Value::Array(definition.values.clone()));
                    schema.insert("title".into(), definition.name.into());
                    if let Some(kind) = json_type(&definition.values) {
                        schema.insert("type".into(), kind.into());
                    }
                    Value::Object(schema)
                })
            }
            Ty::Model(model) => {
                let model = model.clone();
                let name = self.def_name(&model);
                self.reference(&name, |generator| generator.model(&model))
            }
            Ty::Alias(name, inner) => {
                let inner = (**inner).clone();
                self.reference(name, |generator| generator.schema(&inner))
            }
            Ty::Tagged { property, variants } => {
                let mut mapping = Map::new();
                let mut branches = Vec::new();
                for (tag, variant) in variants {
                    let reference = self.schema(variant);
                    mapping.insert((*tag).to_string(), reference["$ref"].clone());
                    branches.push(reference);
                }
                json!({"discriminator": {"mapping": mapping, "propertyName": property}, "oneOf": branches})
            }
            Ty::Constrained(inner, keywords) => {
                let mut schema = self.schema(inner);
                if let Value::Object(map) = &mut schema {
                    for (keyword, bound) in keywords {
                        map.insert((*keyword).to_string(), bound.clone());
                    }
                }
                schema
            }
        }
    }

    /// The `anyOf` branches of a union. pydantic flattens a union of unions, `None` included, into one `anyOf`.
    fn branches(&mut self, options: &[Ty]) -> Vec<Value> {
        let mut branches = Vec::new();
        for option in options {
            match self.schema(option) {
                Value::Object(mut map) if map.len() == 1 && map.contains_key("anyOf") => {
                    if let Some(Value::Array(inner)) = map.remove("anyOf") {
                        branches.extend(inner);
                    }
                }
                other => branches.push(other),
            }
        }
        branches
    }

    fn model(&mut self, model: &ModelTy) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        let serializing = self.mode == Mode::Serialization;
        for field in model.fields.iter().filter(|field| !(serializing && field.exclude)) {
            let mut schema = match self.schema(&field.ty) {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            if field.ty.titled() {
                schema.insert("title".into(), text::title(field.key()).replace('_', " ").into());
            }
            if let Some(description) = &field.description {
                schema.insert("description".into(), description.clone().into());
            }
            if let Some(default) = &field.default {
                schema.insert("default".into(), default.clone());
            }
            if serializing || field.is_required() {
                required.push(Value::from(field.key()));
            }
            properties.insert(field.key().to_string(), Value::Object(schema));
        }
        // pydantic's own order, which the final sort keeps for a model defined under the name `default`.
        let mut schema = Map::new();
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        schema.insert("title".into(), model.name.clone().into());
        if let Some(doc) = &model.doc {
            schema.insert("description".into(), doc.clone().into());
        }
        if model.forbid_extra {
            schema.insert("additionalProperties".into(), false.into());
        }
        Value::Object(schema)
    }
}

/// The JSON type every value of a closed set shares, as pydantic names it for `Literal` and enums.
fn json_type(values: &[Value]) -> Option<&'static str> {
    let kind = |value: &Value| match value {
        Value::String(_) => "string",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::Array(_) => "array",
        Value::Null => "null",
        Value::Object(_) => "object",
    };
    let first = kind(values.first()?);
    (values.iter().all(|v| kind(v) == first) && first != "object").then_some(first)
}

fn literal(values: &[Value]) -> Value {
    let mut schema = Map::new();
    if values.len() == 1 {
        schema.insert("const".into(), values[0].clone());
    } else {
        schema.insert("enum".into(), Value::Array(values.to_vec()));
    }
    if let Some(kind) = json_type(values) {
        schema.insert("type".into(), kind.into());
    }
    Value::Object(schema)
}

/// pydantic's final pass: every key sorted, except the names under `properties` and the contents of a `default`.
/// The parent key is passed down through lists, and a property's schema is sorted under its own name, so a property
/// called `default` or `properties` keeps its keys in written order, as pydantic does.
fn sort(value: Value, parent: Option<&str>) -> Value {
    match value {
        Value::Object(map) => {
            let keep = matches!(parent, Some("properties" | "default"));
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            if !keep {
                entries.sort_by(|a, b| a.0.cmp(&b.0));
            }
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| {
                        let item = sort(item, Some(&key));
                        (key, item)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(|item| sort(item, parent)).collect()),
        other => other,
    }
}

/// Joins a flattened optional object's name to its field's: `cite__first`.
const FLAT: &str = "__";

/// Every `anyOf [object, null]` property as one nullable scalar per leaf field, `cite` as `cite__first` and
/// `cite__last`, which [`nest_flat_objects`] folds back into the object.
///
/// Some providers' constrained decoders answer `null` for every optional object while optional scalars decode fine. On
/// OpenRouter, 12 of 21 DeepSeek V4.1 Flash endpoints left every read claim uncited, so code rejected each one; with
/// the cite flattened, the 6 of them tried again cited every claim.
pub fn flat_optional_objects(schema: &Value) -> Value {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    flatten(schema, &defs)
}

fn flatten(node: &Value, defs: &Value) -> Value {
    match node {
        Value::Array(items) => Value::Array(items.iter().map(|item| flatten(item, defs)).collect()),
        Value::Object(map) => {
            let mut result = Map::new();
            let mut renamed: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (key, value) in map {
                let value = match value {
                    // These map names to schemas, so a property called "anyOf" is a name, not a keyword.
                    Value::Object(names) if key == "properties" || key == "$defs" => {
                        let mut flat = Map::new();
                        for (name, item) in names {
                            match optional_object(item, defs).filter(|_| key == "properties") {
                                Some(object) => {
                                    let before = flat.len();
                                    leaves(
                                        name,
                                        object,
                                        item.get("description").and_then(Value::as_str).map(str::to_string),
                                        defs,
                                        &mut flat,
                                    );
                                    renamed.insert(name.clone(), flat.keys().skip(before).cloned().collect());
                                }
                                None => {
                                    flat.insert(name.clone(), flatten(item, defs));
                                }
                            }
                        }
                        Value::Object(flat)
                    }
                    other => flatten(other, defs),
                };
                result.insert(key.clone(), value);
            }
            if let Some(Value::Array(required)) = result.get_mut("required") {
                *required = required
                    .iter()
                    .flat_map(|name| match name.as_str().and_then(|name| renamed.get(name)) {
                        Some(leaves) => leaves.iter().cloned().map(Value::from).collect(),
                        None => vec![name.clone()],
                    })
                    .collect();
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// The object schema a property holds, through a `$ref` into `$defs`.
fn object_schema<'a>(node: &'a Value, defs: &'a Value) -> Option<&'a Map<String, Value>> {
    let node = match node.get("$ref").and_then(Value::as_str) {
        Some(path) => defs.get(path.strip_prefix("#/$defs/")?)?,
        None => node,
    };
    node.get("properties").and(node.as_object())
}

/// The object an `anyOf [object, null]` property holds when present.
fn optional_object<'a>(node: &'a Value, defs: &'a Value) -> Option<&'a Map<String, Value>> {
    let [first, second] = node.get("anyOf")?.as_array()?.as_slice() else { return None };
    let is_null = |branch: &Value| branch.get("type").and_then(Value::as_str) == Some("null");
    let present = if is_null(second) {
        first
    } else if is_null(first) {
        second
    } else {
        return None;
    };
    object_schema(present, defs)
}

fn leaves(
    prefix: &str,
    object: &Map<String, Value>,
    mut description: Option<String>,
    defs: &Value,
    flat: &mut Map<String, Value>,
) {
    let Some(Value::Object(fields)) = object.get("properties") else { return };
    for (field, schema) in fields {
        let name = format!("{prefix}{FLAT}{field}");
        if let Some(inner) = object_schema(schema, defs).or_else(|| optional_object(schema, defs)) {
            leaves(&name, inner, description.take(), defs, flat);
            continue;
        }
        let Value::Object(mut leaf) = flatten(schema, defs) else {
            flat.insert(name, schema.clone());
            continue;
        };
        let own = leaf.remove("description").and_then(|d| d.as_str().map(str::to_string));
        let nullable = leaf.get("anyOf").and_then(Value::as_array).is_some_and(|branches| {
            branches.iter().any(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
        });
        let mut wrapped =
            if nullable { leaf } else { Map::from_iter([("anyOf".to_string(), json!([leaf, {"type": "null"}]))]) };
        let text = [description.take(), own].into_iter().flatten().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            wrapped.insert("description".into(), text.into());
        }
        flat.insert(name, Value::Object(wrapped));
    }
}

/// The reply to a [`flat_optional_objects`] schema folded back into `schema`, the one it was flattened from. An
/// optional object comes back `null` when every leaf is, or when a field it requires is: the original schema allows
/// no object there, but not half of one, and a partial `comparison` or tally `field` used to fail validation twice
/// and end the run. The path of each half-answered object set to `null` is returned beside the value, for the trace.
pub fn nest_flat_objects(schema: &Value, value: Value) -> (Value, Vec<String>) {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    let mut dropped = Vec::new();
    let nested = nest(value, schema, &defs, "", &mut dropped);
    (nested, dropped)
}

fn nest(value: Value, node: &Value, defs: &Value, path: &str, dropped: &mut Vec<String>) -> Value {
    let at = |key: &str| if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
    match value {
        Value::Array(items) => {
            let item = items_schema(node, defs).unwrap_or(&Value::Null);
            Value::Array(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| nest(value, item, defs, &at(&i.to_string()), dropped))
                    .collect(),
            )
        }
        Value::Object(map) => {
            let properties = object_schema(node, defs)
                .or_else(|| optional_object(node, defs))
                .and_then(|object| object.get("properties"))
                .unwrap_or(&Value::Null);
            let mut result = Map::new();
            let mut grouped: Vec<String> = Vec::new();
            for (key, value) in map {
                let Some((head, rest)) = key.split_once(FLAT) else {
                    let value = nest(value, properties.get(&key).unwrap_or(&Value::Null), defs, &at(&key), dropped);
                    result.insert(key, value);
                    continue;
                };
                if !grouped.iter().any(|name| name == head) {
                    grouped.push(head.to_string());
                    result.insert(head.to_string(), Value::Object(Map::new()));
                }
                if let Some(Value::Object(fields)) = result.get_mut(head) {
                    fields.insert(rest.to_string(), value);
                }
            }
            for head in grouped {
                let field = properties.get(&head).unwrap_or(&Value::Null);
                if let Some(slot) = result.get_mut(&head) {
                    let nested = nest(slot.take(), field, defs, &at(&head), dropped);
                    *slot = if all_null(&nested) {
                        Value::Null
                    } else if !complete(&nested, field, defs) {
                        dropped.push(at(&head));
                        Value::Null
                    } else {
                        nested
                    };
                }
            }
            Value::Object(result)
        }
        other => other,
    }
}

/// The schema of an array's items, through a `$ref` and an `anyOf [array, null]`.
fn items_schema<'a>(node: &'a Value, defs: &'a Value) -> Option<&'a Value> {
    let node = match node.get("$ref").and_then(Value::as_str) {
        Some(path) => defs.get(path.strip_prefix("#/$defs/")?)?,
        None => node,
    };
    node.get("items").or_else(|| node.get("anyOf")?.as_array()?.iter().find_map(|branch| items_schema(branch, defs)))
}

/// Whether `value` has every field its object schema requires, `null` only where that field allows it.
fn complete(value: &Value, node: &Value, defs: &Value) -> bool {
    let Some(object) = object_schema(node, defs).or_else(|| optional_object(node, defs)) else { return true };
    let mut required = object.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    required.all(|name| {
        let allows_null = object["properties"].get(name).is_some_and(|field| {
            field.get("type").and_then(Value::as_str) == Some("null")
                || field.get("anyOf").and_then(Value::as_array).is_some_and(|branches| {
                    branches.iter().any(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
                })
        });
        allows_null || !value.get(name).is_none_or(Value::is_null)
    })
}

fn all_null(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(fields) => fields.values().all(all_null),
        _ => false,
    }
}

/// Why a schema cannot go out as a strict one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a strict schema cannot hold a map with free keys; use a list of keyed entries")]
pub struct FreeKeys;

/// Every property required and no defaults.
///
/// Strict mode refuses an optional property, so validation fills a default instead. Every constraint validation
/// enforces stays in the schema, so the model is told each rule its reply is held to. A map with free keys is
/// refused: Gemini through the Vercel AI Gateway returned one empty where OpenRouter filled it.
pub fn strict_schema(schema: &Value) -> Result<Value, FreeKeys> {
    match schema {
        Value::Array(items) => items.iter().map(strict_schema).collect::<Result<Vec<_>, _>>().map(Value::Array),
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                if key == "default" {
                    continue;
                }
                if key == "additionalProperties" && value.is_object() {
                    return Err(FreeKeys);
                }
                // These map names to schemas, so a property called "default" is a name, not a keyword.
                let strict = match value {
                    Value::Object(names) if key == "properties" || key == "$defs" => Value::Object(
                        names
                            .iter()
                            .map(|(name, item)| strict_schema(item).map(|s| (name.clone(), s)))
                            .collect::<Result<Map<_, _>, _>>()?,
                    ),
                    other => strict_schema(other)?,
                };
                result.insert(key.clone(), strict);
            }
            if let Some(Value::Object(properties)) = map.get("properties") {
                result.insert("required".into(), properties.keys().cloned().map(Value::from).collect());
            }
            Ok(Value::Object(result))
        }
        other => Ok(other.clone()),
    }
}
