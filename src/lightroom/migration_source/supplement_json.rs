//! Private supplement view, preserving the pinned `serde_json::Value` input
//! semantics without retaining unrelated metadata trees. The original bytes
//! remain authority. This is not a general JSON or NativePath replacement.
use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use std::fmt;

use crate::xmp_packets::{SourceRevision, Status};

// serde_json's enabled raw_value feature recognizes this key only in the first
// position of a Value map. Preserve that existing behavior even in ignored data.
const RAW_VALUE: &str = "$serde_json::private::RawValue";

#[derive(Clone, Copy)]
enum Shape {
    Root,
    Inspections,
    Row,
    Revision,
    Status,
    Scalar,
    Ignore,
}

#[derive(Clone, Copy)]
struct View<'a> {
    shape: Shape,
    origin: &'a str,
    stop: &'a dyn Fn() -> bool,
}

impl View<'_> {
    fn child(self, shape: Shape) -> Self {
        Self { shape, ..self }
    }

    fn check<E: de::Error>(self) -> std::result::Result<(), E> {
        if (self.stop)() {
            Err(E::custom("supplement read canceled"))
        } else {
            Ok(())
        }
    }

    fn scalar(self, value: Value) -> Value {
        match self.shape {
            Shape::Revision | Shape::Status | Shape::Scalar => value,
            _ => Value::Null,
        }
    }

    fn field(self, key: &str) -> Shape {
        match (self.shape, key) {
            (Shape::Root, "inspections") => Shape::Inspections,
            (Shape::Row, "origin") => Shape::Scalar,
            (Shape::Row, "revision") => Shape::Revision,
            (Shape::Row, "status") => Shape::Status,
            (Shape::Revision, "length" | "blake3" | "modified_unix_ns") => Shape::Scalar,
            (Shape::Status, _) => Shape::Scalar,
            _ => Shape::Ignore,
        }
    }
}

impl<'de> DeserializeSeed<'de> for View<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        de: D,
    ) -> std::result::Result<Value, D::Error> {
        self.check()?;
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for View<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any valid JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(self.scalar(Value::Bool(value)))
    }
    fn visit_i64<E>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(self.scalar(Value::Number(value.into())))
    }
    fn visit_u64<E>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(self.scalar(Value::Number(value.into())))
    }
    fn visit_f64<E>(self, value: f64) -> std::result::Result<Value, E> {
        // Match Value, including its Number conversion, rather than directly
        // deserializing a SourceRevision u128 from the original number token.
        Ok(self.scalar(Number::from_f64(value).map_or(Value::Null, Value::Number)))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(match self.shape {
            Shape::Revision | Shape::Status | Shape::Scalar => Value::String(value.to_owned()),
            _ => Value::Null,
        })
    }
    fn visit_unit<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        loop {
            self.check()?;
            let shape = match self.shape {
                Shape::Inspections => Shape::Row,
                Shape::Revision if values.len() < 4 => Shape::Scalar,
                _ => Shape::Ignore,
            };
            let Some(value) = seq.next_element_seed(self.child(shape))? else {
                break;
            };
            match self.shape {
                Shape::Inspections
                    if values.len() < 2
                        && value.get("origin").and_then(Value::as_str) == Some(self.origin) =>
                {
                    values.push(value)
                }
                Shape::Revision if values.len() < 4 => values.push(value),
                _ => {}
            }
        }
        // Empty containers still convey the original invalid type to the
        // scalar/enum deserializer. No unrelated array members are retained.
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let Some(mut key) = map.next_key::<String>()? else {
            return Ok(Value::Object(Map::new()));
        };
        if key == RAW_VALUE {
            // This is also Value's first-key path. Let MapAccess's enclosing
            // deserializer reject trailing entries as before; do not drain them.
            return map.next_value_seed(Raw(self));
        }
        let mut values = Map::new();
        loop {
            self.check()?;
            let shape = self.field(&key);
            let value = map.next_value_seed(self.child(shape))?;
            if !matches!(shape, Shape::Ignore)
                && (!matches!(self.shape, Shape::Status)
                    || values.len() < 2
                    || values.contains_key(&key))
            {
                // Value's object duplicate behavior is last-wins, including
                // discarded inspections lists and previously invalid fields.
                values.insert(key, value);
            }
            let Some(next) = map.next_key()? else { break };
            key = next;
        }
        Ok(Value::Object(values))
    }
}

// Parse the reserved payload while borrowing the parent parser's decoded
// string. Unlike Value's Box<RawValue> path, no second owned string is needed.
// Nested parsers' scratch buffers can coexist and are charged separately.
struct Raw<'a>(View<'a>);
impl<'de> DeserializeSeed<'de> for Raw<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        de: D,
    ) -> std::result::Result<Value, D::Error> {
        de.deserialize_str(self)
    }
}
impl Visitor<'_> for Raw<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("raw value")
    }
    fn visit_str<E: de::Error>(self, encoded: &str) -> std::result::Result<Value, E> {
        let mut de = serde_json::Deserializer::from_str(encoded);
        let value = self.0.deserialize(&mut de).map_err(E::custom)?;
        de.end().map_err(E::custom)?;
        Ok(value)
    }
}

fn project(bytes: &[u8], origin: &str, stop: &dyn Fn() -> bool) -> Result<Value> {
    ensure!(
        bytes.len() <= crate::lightroom::PAGE_BYTES,
        "supplement baseline metadata limit"
    );
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let evidence = View {
        shape: Shape::Root,
        origin,
        stop,
    }
    .deserialize(&mut de)?;
    de.end()?;
    ensure!(!stop(), "supplement read canceled");
    Ok(evidence)
}

pub(super) fn select(
    bytes: &[u8],
    origin: &str,
    stop: &dyn Fn() -> bool,
) -> Result<(SourceRevision, Status)> {
    let evidence = project(bytes, origin, stop)?;
    let matches = evidence
        .get("inspections")
        .and_then(Value::as_array)
        .context("supplement has no retained original inspection")?;
    ensure!(
        matches.len() == 1,
        "supplement original association missing or ambiguous"
    );
    let source = SourceRevision::deserialize(
        matches[0]
            .get("revision")
            .context("supplement source revision missing")?,
    )?;
    let status = Status::deserialize(
        matches[0]
            .get("status")
            .context("supplement status missing")?,
    )?;
    Ok((source, status))
}

#[cfg(test)]
#[path = "supplement_json/tests.rs"]
mod tests;
