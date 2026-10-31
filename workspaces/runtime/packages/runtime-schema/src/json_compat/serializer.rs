//! One-pass serialization for Python-compatible JSON spellings.
use super::{python_float_json, python_quote};
use serde::ser::{
    self, Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, serde_json::Error>;
#[derive(Clone, Copy)]
struct Serializer<'a> {
    item: &'a str,
    key_separator: &'a str,
    ascii: bool,
    key: bool,
}

pub(super) fn serialize<T: Serialize + ?Sized>(
    value: &T,
    item: &str,
    key_separator: &str,
    ascii: bool,
) -> Result<String> {
    value.serialize(Serializer {
        item,
        key_separator,
        ascii,
        key: false,
    })
}
fn invalid_key() -> serde_json::Error {
    ser::Error::custom("key must be a string")
}

struct Array<'a> {
    serializer: Serializer<'a>,
    values: Vec<String>,
    variant: Option<&'static str>,
}
struct Object<'a> {
    serializer: Serializer<'a>,
    fields: BTreeMap<String, String>,
    pending: Option<String>,
    variant: Option<&'static str>,
}
impl<'a> Serializer<'a> {
    fn value(self) -> Self {
        Self { key: false, ..self }
    }
    fn key(self) -> Self {
        Self { key: true, ..self }
    }
    fn object(self, variant: Option<&'static str>) -> Result<Object<'a>> {
        if self.key {
            return Err(invalid_key());
        }
        Ok(Object {
            serializer: self,
            fields: BTreeMap::new(),
            pending: None,
            variant,
        })
    }
    fn array(self, variant: Option<&'static str>) -> Result<Array<'a>> {
        if self.key {
            return Err(invalid_key());
        }
        Ok(Array {
            serializer: self,
            values: Vec::new(),
            variant,
        })
    }
    fn quoted(self, value: &str) -> String {
        if self.key {
            value.to_owned()
        } else {
            python_quote(value, self.ascii)
        }
    }
    fn variant(self, name: &str, value: &str) -> String {
        format!(
            "{{{}{separator}{value}}}",
            python_quote(name, self.ascii),
            separator = self.key_separator
        )
    }
}
macro_rules! integer_methods { ($($method:ident($ty:ty)),*) => {$ (fn $method(self, v: $ty) -> Result<String> { Ok(v.to_string()) })*}; }
impl<'a> ser::Serializer for Serializer<'a> {
    type Ok = String;
    type Error = serde_json::Error;
    type SerializeSeq = Array<'a>;
    type SerializeTuple = Array<'a>;
    type SerializeTupleStruct = Array<'a>;
    type SerializeTupleVariant = Array<'a>;
    type SerializeMap = Object<'a>;
    type SerializeStruct = Object<'a>;
    type SerializeStructVariant = Object<'a>;
    fn serialize_bool(self, v: bool) -> Result<String> {
        Ok(v.to_string())
    }
    integer_methods!(
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64)
    );
    fn serialize_i128(self, v: i128) -> Result<String> {
        if !self.key && i64::try_from(v).is_err() && u64::try_from(v).is_err() {
            return Err(ser::Error::custom("number out of range"));
        }
        Ok(v.to_string())
    }
    fn serialize_u128(self, v: u128) -> Result<String> {
        if !self.key && u64::try_from(v).is_err() {
            return Err(ser::Error::custom("number out of range"));
        }
        Ok(v.to_string())
    }
    fn serialize_f32(self, v: f32) -> Result<String> {
        if self.key && v.is_finite() {
            serde_json::to_string(&v)
        } else {
            self.serialize_f64(f64::from(v))
        }
    }
    fn serialize_f64(self, v: f64) -> Result<String> {
        if !v.is_finite() {
            if self.key {
                Err(invalid_key())
            } else {
                Ok("null".into())
            }
        } else if self.key {
            serde_json::to_string(&v)
        } else {
            Ok(python_float_json(v))
        }
    }
    fn serialize_char(self, v: char) -> Result<String> {
        self.serialize_str(&v.to_string())
    }
    fn serialize_str(self, v: &str) -> Result<String> {
        Ok(self.quoted(v))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<String> {
        let mut seq = self.serialize_seq(Some(v.len()))?;
        for b in v {
            seq.push(b)?;
        }
        Ok(seq.finish())
    }
    fn serialize_none(self) -> Result<String> {
        if self.key {
            Err(invalid_key())
        } else {
            Ok("null".into())
        }
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<String> {
        if self.key {
            Err(invalid_key())
        } else {
            v.serialize(self)
        }
    }
    fn serialize_unit(self) -> Result<String> {
        self.serialize_none()
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<String> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<String> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        v: &T,
    ) -> Result<String> {
        if name == "$serde_json::private::RawValue" {
            return Err(ser::Error::custom(
                "raw JSON cannot be canonicalized without decoding",
            ));
        }
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<String> {
        if self.key {
            return Err(invalid_key());
        }
        Ok(self.variant(variant, &v.serialize(self.value())?))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Array<'a>> {
        self.array(None)
    }
    fn serialize_tuple(self, _: usize) -> Result<Array<'a>> {
        self.array(None)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Array<'a>> {
        self.array(None)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Array<'a>> {
        self.array(Some(variant))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Object<'a>> {
        self.object(None)
    }
    fn serialize_struct(self, name: &'static str, _: usize) -> Result<Object<'a>> {
        if name == "$serde_json::private::RawValue" {
            return Err(ser::Error::custom(
                "raw JSON cannot be canonicalized without decoding",
            ));
        }
        self.object(None)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Object<'a>> {
        self.object(Some(variant))
    }
}
impl Array<'_> {
    fn push<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        self.values.push(value.serialize(self.serializer.value())?);
        Ok(())
    }
    fn finish(self) -> String {
        let result = format!("[{}]", self.values.join(self.serializer.item));
        match self.variant {
            Some(name) => self.serializer.variant(name, &result),
            None => result,
        }
    }
}
impl SerializeSeq for Array<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.push(v)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl SerializeTuple for Array<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.push(v)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl SerializeTupleStruct for Array<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.push(v)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl SerializeTupleVariant for Array<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        self.push(v)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl Object<'_> {
    fn insert<T: Serialize + ?Sized>(&mut self, key: String, value: &T) -> Result<()> {
        self.fields
            .insert(key, value.serialize(self.serializer.value())?);
        Ok(())
    }
    fn finish(self) -> String {
        let fields = self
            .fields
            .into_iter()
            .map(|(k, v)| {
                format!(
                    "{}{separator}{v}",
                    python_quote(&k, self.serializer.ascii),
                    separator = self.serializer.key_separator
                )
            })
            .collect::<Vec<_>>()
            .join(self.serializer.item);
        let result = format!("{{{fields}}}");
        match self.variant {
            Some(name) => self.serializer.variant(name, &result),
            None => result,
        }
    }
}
impl SerializeMap for Object<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        self.pending = Some(key.serialize(self.serializer.key())?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let key = self
            .pending
            .take()
            .ok_or_else(|| ser::Error::custom("missing map key"))?;
        self.insert(key, value)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl SerializeStruct for Object<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.insert(key.to_owned(), value)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
impl SerializeStructVariant for Object<'_> {
    type Ok = String;
    type Error = serde_json::Error;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.insert(key.to_owned(), value)
    }
    fn end(self) -> Result<String> {
        Ok(self.finish())
    }
}
