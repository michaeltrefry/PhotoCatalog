//! Method-bound reply projection. Borrowed spans are inspected before retaining
//! repeated graphs. No input bytes become filesystem or destination authority.
use super::super::transport::{Authority, Read};
use super::{Query, Value};
use crate::{
    application::I64,
    lightroom::{
        migration_source::{
            self as source, Field, ImageLinks, ReadLimits, Resolution, StableSource,
        },
        plan::Cell,
    },
};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::value::RawValue;
use source::manifest_json::{array, units, units_bounded};
use source::record_json::{RawField, cell, exact_text, page, text};
#[cfg(test)]
use source::{ByteRef, Collection, Cursor, EvidenceRecord, Page, record_json::size};

#[derive(Clone, Copy)]
pub(in crate::lightroom_migration_worker::source_reader) enum Budget {
    Sql(ReadLimits),
    Raw(usize),
    Capture(super::super::capture_wire::Limits),
}
impl Budget {
    pub(in crate::lightroom_migration_worker::source_reader) fn from_authority(
        a: &Authority,
    ) -> Result<Self> {
        Ok(match a {
            Authority::Sql { limits, .. } => {
                let limits: ReadLimits = (*limits).try_into()?;
                limits.validate()?;
                Self::Sql(limits)
            }
            Authority::Artifact { limits, .. } => {
                let limits: crate::catalog_migration::artifacts::ArtifactLimits =
                    (*limits).try_into()?;
                limits.validate()?;
                Self::Raw(limits.chunk_bytes)
            }
            Authority::CaptureSql { value } => {
                value.limits.validate()?;
                Self::Capture(value.limits)
            }
        })
    }
}
pub(in crate::lightroom_migration_worker::source_reader) struct Expected<'a> {
    pub(in crate::lightroom_migration_worker::source_reader) read: &'a Read,
    pub(in crate::lightroom_migration_worker::source_reader) budget: Budget,
    pub(in crate::lightroom_migration_worker::source_reader) binding: &'a str,
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
        Read::CaptureSql(query) => {
            let Budget::Capture(limits) = expected.budget else {
                anyhow::bail!("source authority/method mismatch")
            };
            ensure!(
                raw.get().len() <= usize::try_from(limits.result_bytes.0)?,
                "CaptureSql result bound"
            );
            match query {
                super::super::capture_wire::Query::SchemaObjects => {
                    Value::CaptureSchemaObjects(serde_json::from_str(raw.get())?)
                }
                super::super::capture_wire::Query::Variables => {
                    #[derive(Deserialize)]
                    #[serde(deny_unknown_fields)]
                    struct Variables {
                        authority_binding: String,
                        schema_roster_blake3: String,
                        values: std::collections::BTreeMap<String, String>,
                    }
                    let v: Variables = serde_json::from_str(raw.get())?;
                    Value::CaptureVariables {
                        authority_binding: v.authority_binding,
                        schema_roster_blake3: v.schema_roster_blake3,
                        values: v.values,
                    }
                }
                super::super::capture_wire::Query::TableRows { .. } => {
                    Value::CaptureTable(serde_json::from_str(raw.get())?)
                }
                super::super::capture_wire::Query::Current => {
                    Value::CaptureCurrent(serde_json::from_str(raw.get())?)
                }
            }
        }
        Read::Sql(query) => {
            let Budget::Sql(limits) = expected.budget else {
                anyhow::bail!("source authority/method mismatch")
            };
            match query {
                Query::CaptureManifest { .. } => Value::Manifest(Box::new(
                    source::manifest_json::decode_reply(raw.get().as_bytes(), stop)?,
                )),
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
