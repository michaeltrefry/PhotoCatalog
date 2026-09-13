use crate::{
    application::U64,
    lightroom::{
        capture::Manifest,
        migration_source::{
            ByteRef, Collection, Cursor, ImageLinks, MigrationRead, Page, Resolution, StableSource,
        },
    },
};
use anyhow::Result;
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
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", deny_unknown_fields)]
pub(super) enum Value {
    Verified,
    Manifest(Manifest),
    StableSource(StableSource),
    OriginPacketRoster(Vec<crate::application::I64>),
    Page(Page),
    Chunk(Vec<u8>),
    Count(U64),
    Resolution(Resolution),
    ImageLinks(ImageLinks),
}
impl Query {
    pub(super) fn read(self, source: &dyn MigrationRead) -> Result<Value> {
        Ok(match self {
            Self::CaptureManifest { revision } => {
                Value::Manifest(source.capture_manifest(&revision)?)
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
