//! Shared bounded retained-record JSON projection. No worker dependency and no I/O.
use super::manifest_json::array;
use super::{ByteRef, Collection, Cursor, EvidenceRecord, Field, Page, ReadLimits};
use crate::lightroom::plan::Cell;
use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use std::{borrow::Cow, collections::BTreeMap, fmt, io};
#[derive(Deserialize)]
#[serde(transparent)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);
pub(crate) fn text(raw: &RawValue, max: usize) -> Result<Cow<'_, str>> {
    let value: Text<'_> = serde_json::from_str(raw.get())?;
    ensure!(value.0.len() <= max, "source text byte admission");
    Ok(value.0)
}
pub(crate) fn exact_text(raw: &RawValue, expected: &str) -> Result<String> {
    let value = text(raw, expected.len())?;
    ensure!(value == expected, "source reply identity differs");
    Ok(value.into_owned())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawByteRef<'a> {
    #[serde(borrow)]
    seal: &'a RawValue,
    #[serde(borrow)]
    revision: &'a RawValue,
    collection: Collection,
    rowid: i64,
    #[serde(borrow)]
    field: &'a RawValue,
    bytes: u64,
    text: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCursor<'a> {
    #[serde(borrow)]
    seal: &'a RawValue,
    #[serde(borrow)]
    revision: &'a RawValue,
    collection: Collection,
    #[serde(borrow)]
    after: &'a RawValue,
}
#[derive(Deserialize)]
struct RawRecord<'a> {
    #[serde(borrow)]
    revision: &'a RawValue,
    collection: Collection,
    rowid: i64,
    #[serde(borrow)]
    key: &'a RawValue,
    #[serde(borrow)]
    fields: &'a RawValue,
}
#[derive(Deserialize)]
struct RawPage<'a> {
    #[serde(borrow)]
    records: &'a RawValue,
    #[serde(borrow)]
    next: Option<&'a RawValue>,
    exhausted: bool,
}
#[derive(Deserialize)]
pub(crate) enum RawField<'a> {
    Inline(#[serde(borrow)] &'a RawValue),
    Bytes(#[serde(borrow)] &'a RawValue),
}

// A concrete adjacent-tag visitor keeps the full Cell payload out of Serde's
// generic Content tree even when content precedes the tag. Unknown outer fields
// retain public IgnoredAny semantics. Map versus sequence tag grammar is kept.
struct RawCell<'a> {
    kind: &'a RawValue,
    value: Option<&'a RawValue>,
    sequence: bool,
}
impl<'de: 'a, 'a> Deserialize<'de> for RawCell<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawCell<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("source Cell")
            }
            fn visit_seq<S: de::SeqAccess<'de>>(
                self,
                mut s: S,
            ) -> std::result::Result<Self::Value, S::Error> {
                let kind = s
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let value = Some(
                    s.next_element()?
                        .ok_or_else(|| de::Error::invalid_length(1, &self))?,
                );
                if s.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(3, &self));
                }
                Ok(RawCell {
                    kind,
                    value,
                    sequence: true,
                })
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(field_identifier)]
                enum K {
                    #[serde(rename = "type")]
                    Kind,
                    #[serde(rename = "value")]
                    Value,
                    #[serde(other)]
                    Other,
                }
                let (mut kind, mut value) = (None, None);
                while let Some(k) = m.next_key()? {
                    match k {
                        K::Kind => {
                            if kind.is_some() {
                                return Err(de::Error::duplicate_field("type"));
                            }
                            kind = Some(m.next_value()?);
                        }
                        K::Value => {
                            if value.is_some() {
                                return Err(de::Error::duplicate_field("value"));
                            }
                            value = Some(m.next_value()?);
                        }
                        K::Other => {
                            m.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(RawCell {
                    kind: kind.ok_or_else(|| de::Error::missing_field("type"))?,
                    value,
                    sequence: false,
                })
            }
        }
        d.deserialize_struct("Cell", &["type", "value"], V)
    }
}
#[derive(Deserialize)]
enum CellTag {
    Null,
    Integer,
    RealBits,
    Text,
    Blob,
}
pub(crate) fn cell(raw: &RawValue, inline: usize) -> Result<Cell> {
    let raw: RawCell<'_> = serde_json::from_str(raw.get())?;
    let tag: CellTag = if raw.sequence {
        let name = text(raw.kind, 8)?;
        match name.as_ref() {
            "Null" => CellTag::Null,
            "Integer" => CellTag::Integer,
            "RealBits" => CellTag::RealBits,
            "Text" => CellTag::Text,
            "Blob" => CellTag::Blob,
            _ => anyhow::bail!("source Cell tag"),
        }
    } else {
        serde_json::from_str(raw.kind.get())?
    };
    let payload = || raw.value.context("source Cell payload required");
    Ok(match tag {
        CellTag::Null => {
            if let Some(value) = raw.value {
                serde_json::from_str::<()>(value.get())?;
            }
            Cell::Null
        }
        CellTag::Integer => Cell::Integer(serde_json::from_str(payload()?.get())?),
        CellTag::RealBits => Cell::RealBits(serde_json::from_str(payload()?.get())?),
        CellTag::Text | CellTag::Blob => {
            let value = text(
                payload()?,
                inline.checked_mul(2).context("inline cap overflow")?,
            )?;
            ensure!(
                value.len().is_multiple_of(2) && value.is_ascii(),
                "source hex bytes"
            );
            let mut out = Vec::new();
            out.try_reserve_exact(value.len() / 2)?;
            for pair in value.as_bytes().as_chunks::<2>().0 {
                out.push(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?);
            }
            if matches!(tag, CellTag::Text) {
                Cell::Text(out)
            } else {
                Cell::Blob(out)
            }
        }
    })
}
fn key(
    raw: &RawValue,
    collection: Collection,
    inline: usize,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<Cell>> {
    let (shape, _, numeric) = collection.transport_shape();
    let mut out = Vec::new();
    out.try_reserve_exact(shape.len())?;
    array(raw, stop, |raw| {
        ensure!(out.len() < shape.len(), "source key arity");
        let value = cell(raw, inline)?;
        ensure!(
            matches!(&value,Cell::Integer(n) if numeric&&*n>0)
                || matches!(&value,Cell::Text(v) if !numeric&&std::str::from_utf8(v).is_ok()),
            "source key type"
        );
        out.push(value);
        Ok(())
    })?;
    ensure!(out.len() == shape.len(), "source key arity");
    Ok(out)
}
fn field(
    raw: &RawValue,
    name: &str,
    revision: &str,
    collection: Collection,
    binding: &str,
    inline: usize,
) -> Result<Field> {
    Ok(match serde_json::from_str(raw.get())? {
        RawField::Inline(value) => Field::Inline(cell(value, inline)?),
        RawField::Bytes(value) => {
            let value: RawByteRef<'_> = serde_json::from_str(value.get())?;
            ensure!(value.collection == collection, "source byte collection");
            Field::Bytes(ByteRef {
                seal: exact_text(value.seal, binding)?,
                revision: exact_text(value.revision, revision)?,
                collection,
                rowid: value.rowid,
                field: exact_text(value.field, name)?,
                bytes: value.bytes,
                text: value.text,
            })
        }
    })
}
fn record(
    raw: &RawValue,
    revision: &str,
    collection: Collection,
    binding: &str,
    inline: usize,
    stop: &dyn Fn() -> bool,
) -> Result<EvidenceRecord> {
    let value: RawRecord<'_> = serde_json::from_str(raw.get())?;
    ensure!(value.collection == collection, "source record collection");
    struct Fields<'a> {
        raw: &'a RawValue,
        revision: &'a str,
        collection: Collection,
        binding: &'a str,
        inline: usize,
        stop: &'a dyn Fn() -> bool,
    }
    impl<'de> Visitor<'de> for Fields<'_> {
        type Value = BTreeMap<String, Field>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded source fields")
        }
        fn visit_map<M: MapAccess<'de>>(
            self,
            mut m: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let (_, names, _) = self.collection.transport_shape();
            let mut out = BTreeMap::new();
            while let Some(name) = m.next_key::<Text<'de>>()? {
                if (self.stop)() {
                    return Err(de::Error::custom("source decoding canceled"));
                }
                if !names.contains(&name.0.as_ref()) {
                    return Err(de::Error::custom("source field name outside collection"));
                }
                let raw = m.next_value::<&RawValue>()?;
                let value = field(
                    raw,
                    &name.0,
                    self.revision,
                    self.collection,
                    self.binding,
                    self.inline,
                )
                .map_err(de::Error::custom)?;
                // Validate each duplicate before replacement, matching BTreeMap.
                out.insert(name.0.into_owned(), value);
            }
            if out.len() != names.len() {
                return Err(de::Error::custom("source field roster incomplete"));
            }
            Ok(out)
        }
    }
    let fields = Fields {
        raw: value.fields,
        revision,
        collection,
        binding,
        inline,
        stop,
    };
    let mut d = serde_json::Deserializer::from_str(fields.raw.get());
    let fields = serde::Deserializer::deserialize_map(&mut d, fields)?;
    d.end()?;
    Ok(EvidenceRecord {
        revision: exact_text(value.revision, revision)?,
        collection,
        rowid: value.rowid,
        key: key(value.key, collection, inline, stop)?,
        fields,
    })
}
fn cursor(
    raw: &RawValue,
    revision: &str,
    collection: Collection,
    binding: &str,
    inline: usize,
    stop: &dyn Fn() -> bool,
) -> Result<Cursor> {
    let v: RawCursor<'_> = serde_json::from_str(raw.get())?;
    ensure!(v.collection == collection, "source cursor collection");
    Ok(Cursor {
        seal: exact_text(v.seal, binding)?,
        revision: exact_text(v.revision, revision)?,
        collection,
        after: key(v.after, collection, inline, stop)?,
    })
}
struct Count {
    bytes: usize,
    cap: usize,
}
impl io::Write for Count {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(b.len())
            .filter(|n| *n <= self.cap)
            .ok_or_else(|| io::Error::other("source canonical byte admission"))?;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) fn size(value: &impl Serialize, cap: usize) -> Result<usize> {
    let mut out = Count { bytes: 0, cap };
    serde_json::to_writer(&mut out, value)?;
    Ok(out.bytes)
}
pub(crate) fn page(
    raw: &RawValue,
    revision: &str,
    collection: Collection,
    limit: usize,
    binding: &str,
    limits: ReadLimits,
    stop: &dyn Fn() -> bool,
) -> Result<Page> {
    ensure!((1..=1000).contains(&limit), "source page requested count");
    let value: RawPage<'_> = serde_json::from_str(raw.get())?;
    let mut count = 0usize;
    let mut bytes = 0usize;
    array(value.records, stop, |raw| {
        ensure!(count < limit, "source page record admission");
        let r = record(
            raw,
            revision,
            collection,
            binding,
            limits.inline_bytes,
            stop,
        )?;
        bytes = bytes
            .checked_add(size(&r, limits.page_bytes)?)
            .context("source page size overflow")?;
        if count > 0 {
            bytes += 1;
        }
        ensure!(bytes <= limits.page_bytes, "source page byte admission");
        count += 1;
        Ok(())
    })?;
    let next = value
        .next
        .map(|v| cursor(v, revision, collection, binding, limits.inline_bytes, stop))
        .transpose()?;
    let mut out = Page {
        records: Vec::new(),
        next,
        exhausted: value.exhausted,
    };
    ensure!(
        bytes
            .checked_add(size(&out, limits.page_bytes)?)
            .is_some_and(|n| n <= limits.page_bytes),
        "source page byte admission"
    );
    out.records.try_reserve_exact(count)?;
    array(value.records, stop, |raw| {
        ensure!(out.records.len() < count, "source page count changed");
        out.records.push(record(
            raw,
            revision,
            collection,
            binding,
            limits.inline_bytes,
            stop,
        )?);
        Ok(())
    })?;
    Ok(out)
}

/// Only the Artifact opening path uses this fixed Captures projection. Shared
/// selected_record callers retain their ordinary complete EvidenceRecord API.
pub(crate) fn capture(
    bytes: &[u8],
    binding: &str,
    maximum: usize,
    stop: &dyn Fn() -> bool,
) -> Result<EvidenceRecord> {
    ensure!(
        bytes.len() <= maximum && !stop(),
        "retained capture byte admission/cancel"
    );
    let raw: &RawValue = serde_json::from_slice(bytes)?;
    let head: RawRecord<'_> = serde_json::from_str(raw.get())?;
    let revision = text(head.revision, 64)?;
    record(raw, &revision, Collection::Captures, binding, maximum, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_captures_keep_full_named_fields_and_reject_unbounded_rosters() -> Result<()> {
        let revision = "a".repeat(64);
        let binding = "b".repeat(64);
        let (_, names, _) = Collection::Captures.transport_shape();
        let r = EvidenceRecord {
            revision: revision.clone(),
            collection: Collection::Captures,
            rowid: 1,
            key: vec![Cell::Text(revision.as_bytes().to_vec())],
            fields: names
                .iter()
                .map(|n| ((*n).to_owned(), Field::Inline(Cell::Text(vec![0, 255]))))
                .collect(),
        };
        let bytes = serde_json::to_vec(&r)?;
        let out = capture(&bytes, &binding, 8 * 1024 * 1024, &|| false)?;
        assert_eq!(serde_json::to_vec(&out)?, bytes);
        assert_eq!(out.key.capacity(), 1);
        let raw = String::from_utf8(bytes)?;
        let malformed = raw.replacen(
            "\"fields\":{",
            "\"fields\":{\"arbitrary\":{\"Inline\":{\"type\":\"Null\"}},",
            1,
        );
        assert!(capture(malformed.as_bytes(), &binding, 8 * 1024 * 1024, &|| false).is_err());
        assert!(capture(raw.as_bytes(), &binding, 8 * 1024 * 1024, &|| true).is_err());
        Ok(())
    }
}
