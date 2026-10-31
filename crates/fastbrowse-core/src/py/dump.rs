//! `model_dump(mode="json")`, with and without `exclude_none`.
//!
//! pydantic's `exclude_none` drops a model's fields that are None and keeps a None held inside a dict or a JSON
//! value: `{"criteria": {"a": null}}` keeps its null. A filter over the finished JSON cannot tell those apart, so
//! this serializer does it while the type is still known: a struct is a model, a map is a dict.

use std::fmt;

use serde::Serialize;
use serde::ser::{self, Impossible};
use serde_json::{Map, Number, Value};

use super::json::{self, Style};

#[derive(Debug)]
pub struct DumpError(String);

impl fmt::Display for DumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DumpError {}

impl ser::Error for DumpError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DumpError(msg.to_string())
    }
}

/// What every model can be dumped as. Implemented for anything serializable; the models' `Serialize` impls are
/// written to produce pydantic's JSON mode.
pub trait Dump: Serialize {
    /// `model_dump(mode="json")`.
    fn dump(&self) -> Value {
        to_value(self, false).expect("a model always dumps")
    }

    /// `model_dump(mode="json", exclude_none=True)`.
    fn dump_exclude_none(&self) -> Value {
        to_value(self, true).expect("a model always dumps")
    }

    /// `model_dump_json()`.
    fn dump_json(&self) -> String {
        json::write(&self.dump(), Style::PYDANTIC)
    }
}

impl<T: Serialize + ?Sized> Dump for T {}

/// `include={...}` on a dumped model: its own fields, in declaration order, whatever order `names` lists them in.
pub fn include(value: Value, names: &[&str]) -> Value {
    match value {
        Value::Object(map) => Value::Object(map.into_iter().filter(|(key, _)| names.contains(&key.as_str())).collect()),
        other => other,
    }
}

/// `exclude={...}` on a dumped model.
pub fn exclude(value: Value, names: &[&str]) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.into_iter().filter(|(key, _)| !names.contains(&key.as_str())).collect())
        }
        other => other,
    }
}

pub fn to_value<T: Serialize + ?Sized>(value: &T, exclude_none: bool) -> Result<Value, DumpError> {
    value.serialize(Dumper { exclude_none })
}

#[derive(Clone, Copy)]
struct Dumper {
    exclude_none: bool,
}

struct Seq {
    dumper: Dumper,
    items: Vec<Value>,
    variant: Option<&'static str>,
}

struct Object {
    dumper: Dumper,
    map: Map<String, Value>,
    key: Option<String>,
    /// A struct is a model, whose None fields `exclude_none` drops; a map is a dict, which keeps them.
    model: bool,
    variant: Option<&'static str>,
}

fn wrap(variant: Option<&'static str>, value: Value) -> Value {
    match variant {
        Some(name) => Value::Object(Map::from_iter([(name.to_string(), value)])),
        None => value,
    }
}

impl ser::Serializer for Dumper {
    type Ok = Value;
    type Error = DumpError;
    type SerializeSeq = Seq;
    type SerializeTuple = Seq;
    type SerializeTupleStruct = Seq;
    type SerializeTupleVariant = Seq;
    type SerializeMap = Object;
    type SerializeStruct = Object;
    type SerializeStructVariant = Object;

    fn serialize_bool(self, v: bool) -> Result<Value, DumpError> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_i16(self, v: i16) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_i32(self, v: i32) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_i64(self, v: i64) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_u8(self, v: u8) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, DumpError> {
        Ok(Value::from(v))
    }
    fn serialize_f32(self, v: f32) -> Result<Value, DumpError> {
        self.serialize_f64(f64::from(v))
    }
    // pydantic writes a non-finite float as null in JSON mode.
    fn serialize_f64(self, v: f64) -> Result<Value, DumpError> {
        Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }
    fn serialize_char(self, v: char) -> Result<Value, DumpError> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, DumpError> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<Value, DumpError> {
        Err(DumpError("bytes have no JSON form; serialize them as base64 or text".into()))
    }
    fn serialize_none(self) -> Result<Value, DumpError> {
        Ok(Value::Null)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Value, DumpError> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, DumpError> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Value, DumpError> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<Value, DumpError> {
        Ok(Value::String(variant.to_string()))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _: &'static str, value: &T) -> Result<Value, DumpError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, DumpError> {
        Ok(wrap(Some(variant), value.serialize(self)?))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Seq, DumpError> {
        Ok(Seq { dumper: self, items: Vec::with_capacity(len.unwrap_or(0)), variant: None })
    }
    fn serialize_tuple(self, len: usize) -> Result<Seq, DumpError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Seq, DumpError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Seq, DumpError> {
        Ok(Seq { dumper: self, items: Vec::with_capacity(len), variant: Some(variant) })
    }
    /// serde's derive opens a struct with a `#[serde(flatten)]` field as a map of unknown length, and that struct
    /// is a model; every map type opens with its length.
    fn serialize_map(self, len: Option<usize>) -> Result<Object, DumpError> {
        Ok(Object { dumper: self, map: Map::new(), key: None, model: len.is_none(), variant: None })
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Object, DumpError> {
        Ok(Object { dumper: self, map: Map::new(), key: None, model: true, variant: None })
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Object, DumpError> {
        Ok(Object { dumper: self, map: Map::new(), key: None, model: true, variant: Some(variant) })
    }
}

impl ser::SerializeSeq for Seq {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DumpError> {
        self.items.push(value.serialize(self.dumper)?);
        Ok(())
    }
    fn end(self) -> Result<Value, DumpError> {
        Ok(wrap(self.variant, Value::Array(self.items)))
    }
}

impl ser::SerializeTuple for Seq {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DumpError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, DumpError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for Seq {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DumpError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, DumpError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleVariant for Seq {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DumpError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, DumpError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeMap for Object {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), DumpError> {
        self.key = Some(key.serialize(KeySerializer)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), DumpError> {
        let key = self.key.take().ok_or_else(|| DumpError("a map value came before its key".into()))?;
        let value = value.serialize(self.dumper)?;
        if !(self.model && self.dumper.exclude_none && value.is_null()) {
            self.map.insert(key, value);
        }
        Ok(())
    }
    fn end(self) -> Result<Value, DumpError> {
        Ok(wrap(self.variant, Value::Object(self.map)))
    }
}

impl ser::SerializeStruct for Object {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), DumpError> {
        let value = value.serialize(self.dumper)?;
        if !(self.dumper.exclude_none && value.is_null()) {
            self.map.insert(key.to_string(), value);
        }
        Ok(())
    }
    fn end(self) -> Result<Value, DumpError> {
        Ok(wrap(self.variant, Value::Object(self.map)))
    }
}

impl ser::SerializeStructVariant for Object {
    type Ok = Value;
    type Error = DumpError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), DumpError> {
        ser::SerializeStruct::serialize_field(self, key, value)
    }
    fn end(self) -> Result<Value, DumpError> {
        ser::SerializeStruct::end(self)
    }
}

/// Map keys are strings in JSON; an enum key writes its value.
struct KeySerializer;

impl ser::Serializer for KeySerializer {
    type Ok = String;
    type Error = DumpError;
    type SerializeSeq = Impossible<String, DumpError>;
    type SerializeTuple = Impossible<String, DumpError>;
    type SerializeTupleStruct = Impossible<String, DumpError>;
    type SerializeTupleVariant = Impossible<String, DumpError>;
    type SerializeMap = Impossible<String, DumpError>;
    type SerializeStruct = Impossible<String, DumpError>;
    type SerializeStructVariant = Impossible<String, DumpError>;

    fn serialize_str(self, v: &str) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<String, DumpError> {
        Ok(variant.to_string())
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _: &'static str, value: &T) -> Result<String, DumpError> {
        value.serialize(self)
    }
    fn serialize_char(self, v: char) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_i64(self, v: i64) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_u64(self, v: u64) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_i32(self, v: i32) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_u32(self, v: u32) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_bool(self, _: bool) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_i8(self, v: i8) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_i16(self, v: i16) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_u8(self, v: u8) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_u16(self, v: u16) -> Result<String, DumpError> {
        Ok(v.to_string())
    }
    fn serialize_f32(self, _: f32) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_f64(self, _: f64) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_none(self) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<String, DumpError> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<String, DumpError> {
        Err(key_error())
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, DumpError> {
        Err(key_error())
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, DumpError> {
        Err(key_error())
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, DumpError> {
        Err(key_error())
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, DumpError> {
        Err(key_error())
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, DumpError> {
        Err(key_error())
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, DumpError> {
        Err(key_error())
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, DumpError> {
        Err(key_error())
    }
}

fn key_error() -> DumpError {
    DumpError("a JSON object key has to be a string".into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use serde_json::json;

    #[derive(Serialize)]
    struct Inner {
        a: Option<u8>,
        b: u8,
    }

    #[derive(Serialize)]
    struct Outer {
        inner: Inner,
        dict: BTreeMap<String, Option<u8>>,
        json: Value,
        gone: Option<u8>,
    }

    #[test]
    fn exclude_none_drops_model_fields_and_keeps_dict_entries() {
        let outer = Outer {
            inner: Inner { a: None, b: 1 },
            dict: BTreeMap::from([("x".to_string(), None)]),
            json: json!({"k": null}),
            gone: None,
        };
        assert_eq!(outer.dump_exclude_none(), json!({"inner": {"b": 1}, "dict": {"x": null}, "json": {"k": null}}));
        assert_eq!(
            outer.dump(),
            json!({"inner": {"a": null, "b": 1}, "dict": {"x": null}, "json": {"k": null}, "gone": null})
        );
    }
}
