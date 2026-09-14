//! Method-bound reply projection. Borrowed spans are inspected before retaining
//! repeated graphs. No input bytes become filesystem or destination authority.
use super::super::transport::{Authority, Read};
use super::{Query, Value};
use crate::{
    application::I64,
    lightroom::{
        migration_source::{
            self as source, ByteRef, Collection, Cursor, EvidenceRecord, Field, ImageLinks, Page,
            ReadLimits, Resolution, StableSource,
        },
        plan::Cell,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};
use serde_json::value::RawValue;
use source::manifest_json::{array, units, units_bounded};
use std::{borrow::Cow, collections::BTreeMap, fmt, io};

#[derive(Clone, Copy)]
pub(in crate::lightroom_migration_worker::source_reader) enum Budget {
    Sql(ReadLimits),
    Raw(usize),
}
impl Budget {
    pub(in crate::lightroom_migration_worker::source_reader) fn from_authority(
        a: &Authority,
    ) -> Result<Self> {
        Ok(match a {
            Authority::Sql { limits, .. } => Self::Sql((*limits).try_into()?),
            Authority::Artifact { limits, .. } => Self::Raw(limits.chunk_bytes.0.try_into()?),
        })
    }
}
pub(in crate::lightroom_migration_worker::source_reader) struct Expected<'a> {
    pub(in crate::lightroom_migration_worker::source_reader) read: &'a Read,
    pub(in crate::lightroom_migration_worker::source_reader) budget: Budget,
    pub(in crate::lightroom_migration_worker::source_reader) binding: &'a str,
}
#[derive(Deserialize)]
#[serde(transparent)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);
fn text(raw: &RawValue, max: usize) -> Result<Cow<'_, str>> {
    let value: Text<'_> = serde_json::from_str(raw.get())?;
    ensure!(value.0.len() <= max, "source text byte admission");
    Ok(value.0)
}
fn exact_text(raw: &RawValue, expected: &str) -> Result<String> {
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
struct RawStable<'a> {
    #[serde(borrow)]
    capture_revision: &'a RawValue,
    #[serde(borrow)]
    table: &'a RawValue,
    #[serde(borrow)]
    source_key: &'a RawValue,
    #[serde(borrow)]
    source_key_blake3: &'a RawValue,
    #[serde(borrow)]
    inspection_source_id: &'a RawValue,
}
#[derive(Deserialize)]
struct RawLinks<'a> {
    #[serde(borrow)]
    image_source_id: &'a RawValue,
    #[serde(borrow)]
    file: &'a RawValue,
    #[serde(borrow)]
    master: &'a RawValue,
    #[serde(borrow)]
    current_develop: &'a RawValue,
    #[serde(borrow)]
    limitations: &'a RawValue,
}
#[derive(Deserialize)]
enum RawResolution<'a> {
    Missing,
    Unique(#[serde(borrow)] &'a RawValue),
    Ambiguous,
}
fn resolution(raw: &RawValue) -> Result<Resolution> {
    Ok(match serde_json::from_str(raw.get())? {
        RawResolution::Missing => Resolution::Missing,
        RawResolution::Ambiguous => Resolution::Ambiguous,
        RawResolution::Unique(value) => Resolution::Unique(text(value, 4096)?.into_owned()),
    })
}
#[derive(Deserialize)]
enum RawField<'a> {
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
fn cell(raw: &RawValue, inline: usize) -> Result<Cell> {
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
            for pair in value.as_bytes().chunks_exact(2) {
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
fn size(value: &impl Serialize, cap: usize) -> Result<usize> {
    let mut out = Count { bytes: 0, cap };
    serde_json::to_writer(&mut out, value)?;
    Ok(out.bytes)
}
fn page(
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

pub(super) fn project(
    raw: &RawValue,
    expected: Expected<'_>,
    stop: &dyn Fn() -> bool,
) -> Result<Value> {
    ensure!(!stop(), "source decoding canceled");
    Ok(match expected.read {
        Read::ArtifactVerify => {
            serde_json::from_str::<()>(raw.get())?;
            Value::Verified
        }
        Read::ArtifactChunk { .. } => {
            let Budget::Raw(cap) = expected.budget else {
                anyhow::bail!("source authority/method mismatch")
            };
            Value::Chunk(chunk(raw, cap, stop)?)
        }
        Read::Sql(query) => {
            let Budget::Sql(limits) = expected.budget else {
                anyhow::bail!("source authority/method mismatch")
            };
            match query {
                Query::CaptureManifest { .. } => Value::Manifest(
                    source::manifest_json::decode_reply(raw.get().as_bytes(), stop)?,
                ),
                Query::ReadChunk { limit, .. } => Value::Chunk(chunk(
                    raw,
                    limits.chunk_bytes.min(limit.0.try_into()?),
                    stop,
                )?),
                Query::Count { .. } => Value::Count(serde_json::from_str(raw.get())?),
                Query::Resolve { .. } => Value::Resolution(resolution(raw)?),
                Query::OriginPacketRoster { .. } => {
                    let mut count = 0usize;
                    let n = array(raw, stop, |item| {
                        ensure!(count < 2048, "source origin packet roster limit");
                        count += 1;
                        serde_json::from_str::<I64>(item.get())?;
                        Ok(())
                    })?;
                    ensure!(n <= 2048, "source origin packet roster limit");
                    let mut out = Vec::new();
                    out.try_reserve_exact(n)?;
                    array(raw, stop, |item| {
                        out.push(serde_json::from_str(item.get())?);
                        Ok(())
                    })?;
                    Value::OriginPacketRoster(out)
                }
                Query::Page {
                    revision,
                    collection,
                    limit,
                    ..
                } => Value::Page(page(
                    raw,
                    revision,
                    *collection,
                    limit.0.try_into()?,
                    expected.binding,
                    limits,
                    stop,
                )?),
                Query::StableSource {
                    revision,
                    source_id,
                } => {
                    let v: RawStable<'_> = serde_json::from_str(raw.get())?;
                    let RawField::Inline(key) = serde_json::from_str(v.source_key.get())? else {
                        anyhow::bail!("stable source requires inline text")
                    };
                    let source_key = cell(key, limits.inline_bytes)?;
                    ensure!(
                        matches!(&source_key, Cell::Text(_)),
                        "stable source requires inline text"
                    );
                    Value::StableSource(StableSource {
                        capture_revision: exact_text(v.capture_revision, revision)?,
                        table: text(v.table, limits.inline_bytes)?.into_owned(),
                        source_key: Field::Inline(source_key),
                        source_key_blake3: text(v.source_key_blake3, 64)?.into_owned(),
                        inspection_source_id: exact_text(v.inspection_source_id, source_id)?,
                    })
                }
                Query::ImageLinks { source_id, .. } => {
                    let v: RawLinks<'_> = serde_json::from_str(raw.get())?;
                    Value::ImageLinks(ImageLinks {
                        image_source_id: exact_text(v.image_source_id, source_id)?,
                        file: resolution(v.file)?,
                        master: resolution(v.master)?,
                        current_develop: resolution(v.current_develop)?,
                        limitations: exact_text(v.limitations, source::IMAGE_LINK_LIMITATIONS)?,
                    })
                }
            }
        }
    })
}
fn chunk(raw: &RawValue, cap: usize, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
    ensure!(cap <= 1024 * 1024, "source chunk configured bound");
    let n = units_bounded::<u8>(raw, stop, None, cap)?.0;
    ensure!(n <= cap, "source chunk byte admission");
    Ok(units(raw, stop, Some(n))?.1)
}

#[cfg(test)]
mod tests;
