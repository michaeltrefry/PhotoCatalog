mod preflight;
use crate::{
    application::U64,
    lightroom::{
        capture::Manifest,
        migration_source::{
            ByteRef, Collection, Cursor, EvidenceRecord, ImageLinks, MigrationRead, Page,
            Resolution, StableSource,
        },
    },
};
use anyhow::{Context, Result, ensure};
pub(super) use preflight::{Budget, Expected};
use serde::{Deserialize, Serialize};

/// The three remaining MigrationRead methods (seal, binding and chunk budget)
/// are exact metadata returned by successful initial admission and cached by
/// the proxy. Every source-dependent method is represented here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum Query {
    CaptureManifest {
        revision: String,
    },
    StableSource {
        revision: String,
        source_id: String,
    },
    OriginPacketRoster {
        revision: String,
        source_id: String,
        origin: String,
    },
    Page {
        revision: String,
        collection: Collection,
        after: Option<Cursor>,
        limit: U64,
    },
    ReadChunk {
        reference: ByteRef,
        offset: U64,
        limit: U64,
    },
    Count {
        revision: String,
        collection: Collection,
    },
    Resolve {
        revision: String,
        source_id: String,
        field: String,
        target_table: String,
    },
    ImageLinks {
        revision: String,
        source_id: String,
    },
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(super) enum Value {
    Verified,
    Manifest(Box<Manifest>),
    StableSource(StableSource),
    OriginPacketRoster(Vec<crate::application::I64>),
    Page(Page),
    Chunk(Vec<u8>),
    Count(U64),
    Resolution(Resolution),
    ImageLinks(ImageLinks),
    CaptureSchemaObjects(super::capture_wire::SchemaObjects),
    CaptureVariables {
        authority_binding: String,
        schema_roster_blake3: String,
        values: std::collections::BTreeMap<String, String>,
    },
    CaptureTable(super::capture_wire::TableValue),
    CaptureCurrent(super::capture_wire::Current),
}
impl Query {
    pub(super) fn read(self, source: &dyn MigrationRead) -> Result<Value> {
        Ok(match self {
            Self::CaptureManifest { revision } => {
                Value::Manifest(Box::new(source.capture_manifest(&revision)?))
            }
            Self::StableSource {
                revision,
                source_id,
            } => Value::StableSource(source.stable_source(&revision, &source_id)?),
            Self::OriginPacketRoster {
                revision,
                source_id,
                origin,
            } => Value::OriginPacketRoster(
                source
                    .origin_packet_roster(&revision, &source_id, &origin)?
                    .into_iter()
                    .map(crate::application::I64)
                    .collect(),
            ),
            Self::Page {
                revision,
                collection,
                after,
                limit,
            } => Value::Page(source.page(
                &revision,
                collection,
                after.as_ref(),
                limit.0.try_into()?,
            )?),
            Self::ReadChunk {
                reference,
                offset,
                limit,
            } => Value::Chunk(source.read_chunk(&reference, offset.0, limit.0.try_into()?)?),
            Self::Count {
                revision,
                collection,
            } => Value::Count(U64(source.count(&revision, collection)?)),
            Self::Resolve {
                revision,
                source_id,
                field,
                target_table,
            } => Value::Resolution(source.resolve(&revision, &source_id, &field, &target_table)?),
            Self::ImageLinks {
                revision,
                source_id,
            } => Value::ImageLinks(source.image_links(&revision, &source_id)?),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(super) enum Kind {
    Verified,
    Manifest,
    StableSource,
    OriginPacketRoster,
    Page,
    Chunk,
    Count,
    Resolution,
    ImageLinks,
    CaptureSchemaObjects,
    CaptureVariables,
    CaptureTable,
    CaptureCurrent,
}
impl Value {
    /// Inspect the borrowed envelope and expected method before allocating its
    /// typed body. A content-before-tag reply must not build a generic Content
    /// graph (or route a different large variant through a small query).
    #[cfg(test)]
    pub(super) fn decode(bytes: &[u8], expected: Kind) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope<'a> {
            kind: Kind,
            #[serde(borrow)]
            value: Option<&'a serde_json::value::RawValue>,
        }
        let envelope: Envelope<'_> = serde_json::from_slice(bytes)?;
        ensure!(
            envelope.kind == expected,
            "source reply does not match requested method"
        );
        if expected == Kind::Verified {
            ensure!(
                envelope.value.is_none(),
                "verified source reply must be unit"
            );
            return Ok(Self::Verified);
        }
        let body = envelope.value.context("source result body required")?.get();
        Ok(match expected {
            Kind::Manifest => Self::Manifest(Box::new(
                crate::lightroom::migration_source::manifest_json::decode_reply(
                    body.as_bytes(),
                    &|| false,
                )?,
            )),
            Kind::StableSource => Self::StableSource(serde_json::from_str(body)?),
            Kind::OriginPacketRoster => Self::OriginPacketRoster(serde_json::from_str(body)?),
            Kind::Page => Self::Page(serde_json::from_str(body)?),
            Kind::Chunk => Self::Chunk(serde_json::from_str(body)?),
            Kind::Count => Self::Count(serde_json::from_str(body)?),
            Kind::Resolution => Self::Resolution(serde_json::from_str(body)?),
            Kind::ImageLinks => Self::ImageLinks(serde_json::from_str(body)?),
            Kind::CaptureSchemaObjects => Self::CaptureSchemaObjects(serde_json::from_str(body)?),
            Kind::CaptureVariables => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Variables {
                    authority_binding: String,
                    schema_roster_blake3: String,
                    values: std::collections::BTreeMap<String, String>,
                }
                let v: Variables = serde_json::from_str(body)?;
                Self::CaptureVariables {
                    authority_binding: v.authority_binding,
                    schema_roster_blake3: v.schema_roster_blake3,
                    values: v.values,
                }
            }
            Kind::CaptureTable => Self::CaptureTable(serde_json::from_str(body)?),
            Kind::CaptureCurrent => Self::CaptureCurrent(serde_json::from_str(body)?),
            Kind::Verified => unreachable!("verified unit handled before body admission"),
        })
    }
    /// Reserve before allocating the declared result buffer. Original length,
    /// public grammar, and per-method caps remain unchanged. This operation
    /// allowance survives Source retirement with its returned public value.
    pub(super) fn allocation(length: usize, expected: Expected<'_>) -> Result<(usize, usize)> {
        use crate::lightroom_migration_worker::memory::layout::{
            add, manifest_dynamic_within, mul, record_dynamic,
        };
        ensure!(
            (1..=super::transport::RESULT_BYTES).contains(&length),
            "source reply transport bound"
        );
        // Pinned serde_json custom errors retain only the immediately previous
        // exact Box<str>. Formatting/shrink plus at most three live deserializer
        // scratches and the raw buffer is <=31L+4C+32, C<=FRAME_BYTES. The
        // remaining fixed allowance covers error/wrapper roots. Partial typed
        // results and earlier caller graphs are charged separately below.
        let transient = add(
            mul(32, length)?,
            mul(8, crate::lightroom_migration_worker::protocol::FRAME_BYTES)?,
        )?;
        let graph = match expected.read {
            super::transport::Read::Sql(Query::CaptureManifest { .. }) => add(
                manifest_dynamic_within(length)?,
                std::mem::size_of::<Manifest>(),
            )?,
            super::transport::Read::Sql(Query::Page { limit, .. }) => {
                let Budget::Sql(_) = expected.budget else {
                    anyhow::bail!("source authority/method mismatch")
                };
                let limit = usize::try_from(limit.0)?;
                ensure!((1..=1000).contains(&limit), "source page requested count");
                // Public page requests admit up to 1000 records. The smaller
                // retention worker batch is not a transport-wide limit. Use the
                // complete announced input for partially decoded rejected pages;
                // the canonical page ceiling is checked only after each record.
                // One additional record-shaped graph conservatively covers the
                // cursor's strings and key vector; its inline root is in Page.
                add(
                    record_dynamic(length, add(limit, 1)?)?,
                    add(
                        mul(limit, std::mem::size_of::<EvidenceRecord>())?,
                        std::mem::size_of::<Page>(),
                    )?,
                )?
            }
            super::transport::Read::Sql(Query::OriginPacketRoster { .. }) => mul(
                2048.min(add(length, 1)? / 2),
                std::mem::size_of::<crate::application::I64>(),
            )?,
            super::transport::Read::ArtifactVerify
            | super::transport::Read::Sql(Query::Count { .. }) => 0,
            super::transport::Read::CaptureSql(_) => {
                // The complete typed CaptureSql graph is bounded by its authority
                // result/page caps and the exact admitted encoded length.
                add(mul(4, length)?, 4096)?
            }
            _ => add(mul(2, length)?, 8)?,
        };
        Ok((transient, graph))
    }

    fn manifest_producer_allocation(additional: usize) -> Result<usize> {
        use crate::lightroom_migration_worker::memory::layout::{add, manifest_dynamic, mul};
        use crate::lightroom_migration_worker::protocol::FRAME_BYTES;
        add(
            add(manifest_dynamic()?, additional)?,
            add(
                mul(32, crate::lightroom::MANIFEST_BYTES)?,
                mul(8, FRAME_BYTES)?,
            )?,
        )
    }

    /// Opening validates selected Manifests while both capture partition sets
    /// remain live, but emits no encoded Manifest result. The per-kind producer
    /// high water takes the maximum of this phase and later read production.
    pub(super) fn sql_opening_allocation(
        limits: crate::lightroom::migration_source::ReadLimits,
    ) -> Result<usize> {
        limits.validate()?;
        Self::manifest_producer_allocation(
            crate::lightroom_migration_worker::memory::layout::capture_partitions()?,
        )
    }

    /// Source-side production precedes the result-length reply, so its allowance
    /// must be obtained before sending Read. These bounds use core limits and
    /// actual closed collection rosters, independently of the later wire length.
    pub(super) fn producer_allocation(expected: Expected<'_>) -> Result<usize> {
        use crate::lightroom::plan::Cell;
        use crate::lightroom_migration_worker::memory::layout::{
            add, content_containers, mul, record_dynamic, vector,
        };
        use crate::lightroom_migration_worker::protocol::FRAME_BYTES;
        let chunk = |bytes| add(mul(5, bytes)?, FRAME_BYTES);
        match expected.read {
            super::transport::Read::ArtifactVerify => Ok(FRAME_BYTES),
            super::transport::Read::ArtifactChunk { .. } => {
                let Budget::Raw(bytes) = expected.budget else {
                    anyhow::bail!("source authority/method mismatch")
                };
                chunk(bytes)
            }
            super::transport::Read::CaptureSql(_) => {
                let Budget::Capture(limits) = expected.budget else {
                    anyhow::bail!("source authority/method mismatch")
                };
                add(
                    mul(6, usize::try_from(limits.result_bytes.0)?)?,
                    FRAME_BYTES,
                )?
            }
            super::transport::Read::Sql(query) => {
                let Budget::Sql(limits) = expected.budget else {
                    anyhow::bail!("source authority/method mismatch")
                };
                limits.validate()?;
                Ok(match query {
                    Query::CaptureManifest { .. } => {
                        Self::manifest_producer_allocation(super::transport::RESULT_BYTES)?
                    }
                    Query::Page {
                        collection, limit, ..
                    } => {
                        let count = usize::try_from(limit.0)?;
                        ensure!((1..=1000).contains(&count), "source page requested count");
                        let (keys, fields, _) = collection.transport_shape();
                        // A next row is built before canonical page admission.
                        // Its complete fixed field/key roster, cursor key clone,
                        // and SQL cursor-value copies may coexist with the page.
                        let extra_units = add(fields.len(), mul(3, keys.len())?)?;
                        add(
                            add(
                                record_dynamic(limits.page_bytes, add(count, 1)?)?,
                                vector::<EvidenceRecord>(count)?,
                            )?,
                            add(
                                mul(extra_units, limits.inline_bytes)?,
                                add(mul(6, limits.page_bytes)?, FRAME_BYTES)?,
                            )?,
                        )?
                    }
                    Query::StableSource { .. } => {
                        // Public stable_source validates a generic Vec<Cell>
                        // from the inline key before canonical equality. One
                        // Content layer, final Cell vector, raw/hex/canonical
                        // buffers and error scratch remain separate terms.
                        let bytes = limits.inline_bytes;
                        add(
                            add(content_containers(bytes, 1)?, vector::<Cell>(bytes)?)?,
                            add(mul(40, bytes)?, mul(8, FRAME_BYTES)?)?,
                        )?
                    }
                    Query::OriginPacketRoster { .. } => add(vector::<i64>(2049)?, FRAME_BYTES)?,
                    Query::ReadChunk { limit, .. } => {
                        chunk(limits.chunk_bytes.min(usize::try_from(limit.0)?))?
                    }
                    Query::Count { .. } | Query::Resolve { .. } | Query::ImageLinks { .. } => {
                        FRAME_BYTES
                    }
                })
            }
        }
    }

    pub(super) fn decode_checked(
        bytes: &[u8],
        expected: Expected<'_>,
        stop: &dyn Fn() -> bool,
    ) -> Result<Self> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope<'a> {
            kind: Kind,
            #[serde(borrow)]
            value: Option<&'a serde_json::value::RawValue>,
        }
        ensure!(
            bytes.len() <= super::transport::RESULT_BYTES,
            "source reply transport bound"
        );
        ensure!(!stop(), "source decoding canceled");
        let envelope: Envelope<'_> = serde_json::from_slice(bytes)?;
        ensure!(
            envelope.kind == expected.read.expected_kind(),
            "source reply does not match requested method"
        );
        if envelope.kind == Kind::Verified {
            ensure!(
                envelope.value.is_none(),
                "verified source reply must be unit"
            );
            return Ok(Self::Verified);
        }
        let result = preflight::project(
            envelope.value.context("source result body required")?,
            expected,
            stop,
        )?;
        ensure!(!stop(), "source decoding canceled");
        Ok(result)
    }
}
