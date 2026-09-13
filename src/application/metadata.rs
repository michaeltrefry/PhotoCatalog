//! Selected-image metadata inspection. Retained bytes never become export authority.
use super::organization::ImageIdentity;
use super::{BridgeError, Cancellation, ErrorCode, I64, Limits, U64, error, native};
use crate::{Catalog, catalog_edits::VariantKey, catalog_images, catalog_migration::history};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

type Result<T> = std::result::Result<T, BridgeError>;
const CHUNK: usize = 16 * 1024;
const INLINE: usize = 512;
const RAW_MAX: usize = crate::xmp::MAX_PACKET_BYTES;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Identity {
        key: VariantKey,
    },
    Fields {
        identity: ImageIdentity,
        after: Option<String>,
        limit: u16,
    },
    Candidates {
        identity: ImageIdentity,
        field: String,
        after: Option<String>,
        limit: u16,
    },
    Sources {
        identity: ImageIdentity,
        after: Option<String>,
        limit: u16,
    },
    Observations {
        identity: ImageIdentity,
        after: Option<String>,
        limit: u16,
    },
    Models {
        identity: ImageIdentity,
        observation: I64,
        after: Option<String>,
        limit: u16,
    },
    Packets {
        identity: ImageIdentity,
        observation: I64,
        after: Option<String>,
        limit: u16,
    },
    Decisions {
        identity: ImageIdentity,
        after: Option<String>,
        limit: u16,
    },
    FileInstances {
        identity: ImageIdentity,
        after: Option<String>,
        limit: u16,
    },
    Resolve {
        key: VariantKey,
        expected_revision: I64,
        field: String,
        model: I64,
    },
    TextChunk {
        identity: ImageIdentity,
        reference: TextReference,
        offset: U64,
        length: u32,
    },
    BlobChunk {
        key: VariantKey,
        source: BlobSource,
        offset: U64,
        length: u32,
    },
    ImportHistory {
        key: VariantKey,
        anchor_json: Option<String>,
        direction: history::Direction,
        after_json: Option<String>,
        limit: u16,
    },
    ImportFields {
        key: VariantKey,
        anchor_json: String,
        role: RecordRole,
        after: String,
        limit: u16,
    },
    ImportChunk {
        key: VariantKey,
        anchor_json: String,
        role: RecordRole,
        field: String,
        offset: U64,
        length: u32,
    },
    AdobeProperties {
        key: VariantKey,
        anchor_json: String,
        column: String,
        settings_path_json: String,
        after: U64,
        limit: u16,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlobSource {
    Packet { observation: I64, ordinal: I64 },
    Model { id: I64 },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceText {
    Kind,
    Display,
    Association,
    Availability,
    Locator,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationText {
    Revision,
    Status,
    Issues,
    Provenance,
    CreatedAt,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelText {
    Descriptor,
    Projection,
    Error,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionText {
    Action,
    Detail,
    CreatedAt,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileInstanceText {
    Provenance,
    ObservedAt,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TextReference {
    Effective {
        field: String,
    },
    Candidate {
        model: I64,
        field: String,
    },
    Source {
        source: I64,
        field: SourceText,
    },
    Observation {
        observation: I64,
        field: ObservationText,
    },
    Model {
        model: I64,
        field: ModelText,
    },
    Packet {
        observation: I64,
        ordinal: I64,
    },
    Decision {
        decision: I64,
        field: DecisionText,
    },
    FileInstance {
        instance: I64,
        field: FileInstanceText,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordRole {
    Row,
    Table,
    Entity,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedText {
    pub bytes: U64,
    pub inline: Option<String>,
    pub reference: TextReference,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub rows: Vec<T>,
    pub next: Option<String>,
    pub scanned: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub value: Option<RetainedText>,
    pub conflicted: bool,
    pub selected_model: Option<I64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub model: I64,
    pub source: I64,
    pub observation: I64,
    pub ordinal: I64,
    pub ambiguous: bool,
    pub semantic_hash: String,
    pub value: RetainedText,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: I64,
    pub kind: RetainedText,
    pub display: RetainedText,
    pub association: RetainedText,
    pub availability: RetainedText,
    pub locator: RetainedText,
    pub observation: Option<I64>,
    pub status: Option<RetainedText>,
    pub issues: Option<RetainedText>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: I64,
    pub source: I64,
    pub revision: RetainedText,
    pub status: RetainedText,
    pub issues: RetainedText,
    pub provenance: RetainedText,
    pub created_at: RetainedText,
    pub current: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: I64,
    pub ordinal: I64,
    pub blob_hash: String,
    pub bytes: U64,
    pub descriptor: RetainedText,
    pub projection: RetainedText,
    pub error: Option<RetainedText>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub ordinal: I64,
    pub blob_hash: String,
    pub bytes: U64,
    pub descriptor: RetainedText,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: I64,
    pub revision: I64,
    pub action: RetainedText,
    pub detail: RetainedText,
    pub created_at: RetainedText,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInstance {
    pub id: I64,
    pub source: I64,
    pub provenance: RetainedText,
    pub observed_at: RetainedText,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub bytes: Vec<u8>,
    pub offset: U64,
    pub total: U64,
    pub next: Option<U64>,
    pub blake3: Option<String>,
    pub verified: bool,
    pub inspected_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedRow {
    pub record: I64,
    pub source_key_json: String,
    pub source_id: String,
    pub entity_record: I64,
    pub table_record: I64,
    pub cells_field: String,
    pub cells_json_bytes: U64,
    pub classification: history::Classification,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub reference_record: I64,
    pub source_id: String,
    pub field: String,
    pub target_table: String,
    pub target_key: String,
    pub source: Option<ImportedRow>,
    pub target: Option<ImportedRow>,
    pub compatibility: history::Compatibility,
    pub anchor_json: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedPage {
    pub key: VariantKey,
    pub input: String,
    pub anchor_json: String,
    pub row: ImportedRow,
    pub relations: Vec<Relation>,
    pub next: Option<String>,
    pub coverage_complete: bool,
    pub keys_complete: bool,
    pub adobe_rendering_equivalent: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedField {
    pub name: String,
    pub representation: String,
    pub bytes: U64,
    pub scalar_json: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdobeProperty {
    pub path_json: String,
    pub namespace: Option<String>,
    pub name: String,
    pub lexical: String,
    pub start: U64,
    pub end: U64,
    pub value_json: String,
    pub disposition: crate::lightroom::adobe::Disposition,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdobePage {
    pub row: ImportedRow,
    pub compatibility: history::Compatibility,
    pub reason: String,
    pub input_json: Option<String>,
    pub coordinate_space: Option<String>,
    pub properties: Page<AdobeProperty>,
    pub missing: Vec<String>,
    pub failure: Option<crate::lightroom::adobe::Failure>,
    pub contribution_json: Option<String>,
    pub adobe_rendering_equivalent: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Identity(ImageIdentity),
    Fields(Page<Field>),
    Candidates(Page<Candidate>),
    Sources(Page<Source>),
    Observations(Page<Observation>),
    Models(Page<Model>),
    Packets(Page<Packet>),
    Decisions(Page<Decision>),
    FileInstances(Page<FileInstance>),
    Changed { revision: I64 },
    Chunk(Chunk),
    ImportHistory(ImportedPage),
    ImportFields(Page<ImportedField>),
    AdobeProperties(AdobePage),
}

fn invalid(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(error(ErrorCode::InvalidRequest, message))
    }
}
fn text(v: &str, empty: bool) -> Result<()> {
    invalid(
        (empty || !v.is_empty()) && v.len() <= 1024 && !v.contains('\0'),
        "metadata text bounds",
    )
}
fn check_cancel(cancel: &Cancellation) -> Result<()> {
    if cancel.is_canceled() {
        Err(error(ErrorCode::Canceled, "metadata inspection canceled"))
    } else {
        Ok(())
    }
}
struct Counter {
    bytes: usize,
    max: usize,
}
impl Write for Counter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b.len() > self.max.saturating_sub(self.bytes) {
            return Err(std::io::Error::other("metadata byte limit"));
        }
        self.bytes += b.len();
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn size<T: Serialize>(v: &T, max: usize) -> Result<usize> {
    let mut c = Counter { bytes: 0, max };
    serde_json::to_writer(&mut c, v)
        .map_err(|_| error(ErrorCode::ResourceLimit, "metadata message byte limit"))?;
    Ok(c.bytes)
}
fn opaque<T: Serialize>(v: &T, max: usize) -> Result<String> {
    size(v, max)?;
    serde_json::to_string(v).map_err(|e| native(e.into()))
}
fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}
fn sql<T>(v: rusqlite::Result<T>) -> Result<T> {
    v.map_err(|e| native(e.into()))
}
fn count(limit: u16, bounds: &Limits) -> Result<usize> {
    invalid(
        limit > 0 && limit <= bounds.page_rows,
        "metadata page limit",
    )?;
    Ok(usize::from(limit))
}
fn budget(bounds: &Limits) -> usize {
    bounds
        .page_bytes
        .min(bounds.reply_bytes.saturating_sub(512))
}
fn checked_image(db: &Connection, identity: &ImageIdentity) -> Result<String> {
    catalog_images::require_image_metadata_identity(db, &identity.clone().into())
        .map_err(native)?;
    Ok(identity.image_id.clone())
}
fn owned_image(db: &Connection, key: &VariantKey) -> Result<String> {
    text(&key.asset_id, false)?;
    text(&key.variant_id, false)?;
    catalog_images::id(db, key).map_err(native)
}
fn observation_owned(db: &Connection, image: &str, observation: i64) -> Result<()> {
    invalid(observation > 0, "observation ID")?;
    invalid(sql(db.query_row("SELECT EXISTS(SELECT 1 FROM metadata_image_observations WHERE image_id=?1 AND observation_id=?2)",params![image,observation],|r|r.get(0)))?,"observation is not owned by selected image")
}

fn text_slice(
    db: &Connection,
    image: &str,
    reference: &TextReference,
    offset: u64,
    length: usize,
) -> Result<Option<(u64, Vec<u8>)>> {
    use rusqlite::types::Value as V;
    let mut args = vec![V::Text(image.into())];
    let (from, expression, condition) = match reference {
        TextReference::Effective { field } => {
            text(field, false)?;
            args.push(V::Text(field.clone()));
            ("metadata_effective", "value", "asset_id=?1 AND field=?2")
        }
        TextReference::Candidate { model, field } => {
            text(field, false)?;
            args.extend([V::Integer(model.0), V::Text(field.clone())]);
            (
                "metadata_values v JOIN metadata_models m ON m.id=v.model_id JOIN metadata_image_observations h ON h.observation_id=m.observation_id",
                "v.value",
                "h.image_id=?1 AND m.id=?2 AND v.field=?3",
            )
        }
        TextReference::Source { source, field } => {
            args.push(V::Integer(source.0));
            (
                "metadata_image_sources a JOIN metadata_sources s ON s.id=a.source_id",
                match field {
                    SourceText::Kind => "s.kind",
                    SourceText::Display => "s.display",
                    SourceText::Association => "a.association",
                    SourceText::Availability => "a.availability",
                    SourceText::Locator => "a.logical_locator",
                },
                "a.image_id=?1 AND a.source_id=?2",
            )
        }
        TextReference::Observation { observation, field } => {
            args.push(V::Integer(observation.0));
            (
                "metadata_observations o JOIN metadata_image_observations h ON h.observation_id=o.id",
                match field {
                    ObservationText::Revision => "o.revision",
                    ObservationText::Status => "o.status",
                    ObservationText::Issues => "o.issues",
                    ObservationText::Provenance => "o.provenance",
                    ObservationText::CreatedAt => "o.created_at",
                },
                "h.image_id=?1 AND o.id=?2",
            )
        }
        TextReference::Model { model, field } => {
            args.push(V::Integer(model.0));
            (
                "metadata_models m JOIN metadata_image_observations h ON h.observation_id=m.observation_id",
                match field {
                    ModelText::Descriptor => "m.descriptor",
                    ModelText::Projection => "m.projection",
                    ModelText::Error => "m.error",
                },
                "h.image_id=?1 AND m.id=?2",
            )
        }
        TextReference::Packet {
            observation,
            ordinal,
        } => {
            args.extend([V::Integer(observation.0), V::Integer(ordinal.0)]);
            (
                "metadata_packets p JOIN metadata_image_observations h ON h.observation_id=p.observation_id",
                "p.descriptor",
                "h.image_id=?1 AND p.observation_id=?2 AND p.ordinal=?3",
            )
        }
        TextReference::Decision { decision, field } => {
            args.push(V::Integer(decision.0));
            (
                "metadata_history",
                match field {
                    DecisionText::Action => "action",
                    DecisionText::Detail => "detail",
                    DecisionText::CreatedAt => "created_at",
                },
                "asset_id=?1 AND id=?2",
            )
        }
        TextReference::FileInstance { instance, field } => {
            args.push(V::Integer(instance.0));
            (
                "metadata_file_instances f JOIN metadata_image_sources a ON a.source_id=f.source_id",
                match field {
                    FileInstanceText::Provenance => "f.provenance",
                    FileInstanceText::ObservedAt => "f.observed_at",
                },
                "a.image_id=?1 AND f.id=?2",
            )
        }
    };
    invalid(
        offset <= i64::MAX as u64 && length <= CHUNK,
        "text chunk bounds",
    )?;
    let start = args.len() + 1;
    args.extend([
        V::Integer(
            i64::try_from(offset)
                .map_err(|e| native(e.into()))?
                .checked_add(1)
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "text offset overflow"))?,
        ),
        V::Integer(length as i64),
    ]);
    // Expressions/relations above are closed enums; caller text is always bound.
    let query = format!(
        "SELECT length(CAST({expression} AS BLOB)),substr(CAST({expression} AS BLOB),?{start},?{}) FROM {from} WHERE {condition}",
        start + 1
    );
    let value = sql(db
        .query_row(&query, rusqlite::params_from_iter(args), |r| {
            Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
        })
        .optional())?
    .ok_or_else(|| {
        error(
            ErrorCode::InvalidRequest,
            "text is not owned by selected image",
        )
    })?;
    match value {
        (Some(total), Some(bytes)) => {
            let total = u64::try_from(total).map_err(|e| native(e.into()))?;
            invalid(offset <= total, "text offset exceeds length")?;
            Ok(Some((total, bytes)))
        }
        (None, None) => Ok(None),
        _ => Err(error(ErrorCode::Native, "invalid retained text state")),
    }
}
fn retained(
    db: &Connection,
    image: &str,
    reference: TextReference,
) -> Result<Option<RetainedText>> {
    Ok(
        text_slice(db, image, &reference, 0, INLINE)?.map(|(bytes, value)| RetainedText {
            bytes: U64(bytes),
            inline: if bytes <= INLINE as u64 {
                String::from_utf8(value).ok()
            } else {
                None
            },
            reference,
        }),
    )
}
fn required(db: &Connection, image: &str, reference: TextReference) -> Result<RetainedText> {
    retained(db, image, reference)?
        .ok_or_else(|| error(ErrorCode::Native, "required retained text is null"))
}
fn finish_page<T: Serialize>(
    rows: Vec<(String, T)>,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<T>> {
    let full = rows.len() == limit;
    let mut page = Page {
        rows: Vec::new(),
        next: None,
        scanned: U64(rows.len() as u64),
    };
    let mut last = None;
    for (cursor, row) in rows {
        page.rows.push(row);
        page.next = Some(cursor.clone());
        if size(&page, budget(bounds).saturating_sub(64)).is_err() {
            page.rows.pop();
            page.next = last;
            if page.rows.is_empty() {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "metadata row exceeds page bytes",
                ));
            }
            return Ok(page);
        }
        last = Some(cursor);
    }
    page.next = if full { last } else { None };
    Ok(page)
}
fn field_page(
    db: &Connection,
    image: &str,
    after: &str,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Field>> {
    text(after, true)?;
    let mut stmt=sql(db.prepare("SELECT field,conflicted,model_id FROM metadata_effective WHERE asset_id=?1 AND field>?2 ORDER BY field LIMIT ?3"))?;
    let keys = sql(
        sql(stmt.query_map(params![image, after, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    let rows = keys
        .into_iter()
        .map(|(name, conflicted, model)| {
            text(&name, false)?;
            Ok((
                name.clone(),
                Field {
                    value: retained(
                        db,
                        image,
                        TextReference::Effective {
                            field: name.clone(),
                        },
                    )?,
                    name,
                    conflicted,
                    selected_model: model.map(I64),
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn source_page(
    db: &Connection,
    image: &str,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Source>> {
    let mut stmt=sql(db.prepare("SELECT source_id,current_observation FROM metadata_image_sources WHERE image_id=?1 AND source_id>?2 ORDER BY source_id LIMIT ?3"))?;
    let keys = sql(
        sql(stmt.query_map(params![image, after, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    let rows = keys
        .into_iter()
        .map(|(id, observation)| {
            let get = |field| {
                required(
                    db,
                    image,
                    TextReference::Source {
                        source: I64(id),
                        field,
                    },
                )
            };
            Ok((
                id.to_string(),
                Source {
                    id: I64(id),
                    kind: get(SourceText::Kind)?,
                    display: get(SourceText::Display)?,
                    association: get(SourceText::Association)?,
                    availability: get(SourceText::Availability)?,
                    locator: get(SourceText::Locator)?,
                    observation: observation.map(I64),
                    status: observation
                        .map(|id| {
                            required(
                                db,
                                image,
                                TextReference::Observation {
                                    observation: I64(id),
                                    field: ObservationText::Status,
                                },
                            )
                        })
                        .transpose()?,
                    issues: observation
                        .map(|id| {
                            required(
                                db,
                                image,
                                TextReference::Observation {
                                    observation: I64(id),
                                    field: ObservationText::Issues,
                                },
                            )
                        })
                        .transpose()?,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn observation_page(
    db: &Connection,
    image: &str,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Observation>> {
    let mut stmt=sql(db.prepare("SELECT h.observation_id,o.source_id,COALESCE(a.current_observation=o.id,0) FROM metadata_image_observations h JOIN metadata_observations o ON o.id=h.observation_id LEFT JOIN metadata_image_sources a ON a.image_id=h.image_id AND a.source_id=o.source_id WHERE h.image_id=?1 AND h.observation_id>?2 ORDER BY h.observation_id LIMIT ?3"))?;
    let keys = sql(
        sql(stmt.query_map(params![image, after, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, bool>(2)?,
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    let rows = keys
        .into_iter()
        .map(|(id, source, current)| {
            let get = |field| {
                required(
                    db,
                    image,
                    TextReference::Observation {
                        observation: I64(id),
                        field,
                    },
                )
            };
            Ok((
                id.to_string(),
                Observation {
                    id: I64(id),
                    source: I64(source),
                    revision: get(ObservationText::Revision)?,
                    status: get(ObservationText::Status)?,
                    issues: get(ObservationText::Issues)?,
                    provenance: get(ObservationText::Provenance)?,
                    created_at: get(ObservationText::CreatedAt)?,
                    current,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn model_page(
    db: &Connection,
    image: &str,
    observation: i64,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Model>> {
    observation_owned(db, image, observation)?;
    let mut stmt=sql(db.prepare("SELECT m.id,m.ordinal,m.blob_hash,b.raw_length FROM metadata_models m JOIN metadata_blobs b ON b.hash=m.blob_hash WHERE m.observation_id=?1 AND m.ordinal>?2 ORDER BY m.ordinal LIMIT ?3"))?;
    let keys = sql(sql(
        stmt.query_map(params![observation, after, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                unsigned(r, 3)?,
            ))
        }),
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    let rows = keys
        .into_iter()
        .map(|(id, ordinal, blob_hash, bytes)| {
            Ok((
                ordinal.to_string(),
                Model {
                    id: I64(id),
                    ordinal: I64(ordinal),
                    blob_hash,
                    bytes: U64(bytes),
                    descriptor: required(
                        db,
                        image,
                        TextReference::Model {
                            model: I64(id),
                            field: ModelText::Descriptor,
                        },
                    )?,
                    projection: required(
                        db,
                        image,
                        TextReference::Model {
                            model: I64(id),
                            field: ModelText::Projection,
                        },
                    )?,
                    error: retained(
                        db,
                        image,
                        TextReference::Model {
                            model: I64(id),
                            field: ModelText::Error,
                        },
                    )?,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn packet_page(
    db: &Connection,
    image: &str,
    observation: i64,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Packet>> {
    observation_owned(db, image, observation)?;
    let mut stmt=sql(db.prepare("SELECT p.ordinal,p.blob_hash,b.raw_length FROM metadata_packets p JOIN metadata_blobs b ON b.hash=p.blob_hash WHERE p.observation_id=?1 AND p.ordinal>?2 ORDER BY p.ordinal LIMIT ?3"))?;
    let keys = sql(sql(
        stmt.query_map(params![observation, after, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, unsigned(r, 2)?))
        }),
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    let rows = keys
        .into_iter()
        .map(|(ordinal, blob_hash, bytes)| {
            Ok((
                ordinal.to_string(),
                Packet {
                    ordinal: I64(ordinal),
                    blob_hash,
                    bytes: U64(bytes),
                    descriptor: required(
                        db,
                        image,
                        TextReference::Packet {
                            observation: I64(observation),
                            ordinal: I64(ordinal),
                        },
                    )?,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn decision_page(
    db: &Connection,
    image: &str,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<Decision>> {
    let mut stmt = sql(db.prepare(
        "SELECT id,revision FROM metadata_history WHERE asset_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
    ))?;
    let keys = sql(
        sql(stmt.query_map(params![image, after, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>(),
    )?;
    let rows = keys
        .into_iter()
        .map(|(id, revision)| {
            let get = |field| {
                required(
                    db,
                    image,
                    TextReference::Decision {
                        decision: I64(id),
                        field,
                    },
                )
            };
            Ok((
                id.to_string(),
                Decision {
                    id: I64(id),
                    revision: I64(revision),
                    action: get(DecisionText::Action)?,
                    detail: get(DecisionText::Detail)?,
                    created_at: get(DecisionText::CreatedAt)?,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCursor {
    identity: String,
    field: String,
    source: i64,
    ordinal: i64,
    advance_source: bool,
}
fn candidate_page(
    db: &Connection,
    identity: &ImageIdentity,
    field: &str,
    after: Option<&str>,
    limit: usize,
    bounds: &Limits,
    cancel: &Cancellation,
) -> Result<Page<Candidate>> {
    text(field, false)?;
    let image = &identity.image_id;
    let binding = blake3::hash(opaque(identity, 8192)?.as_bytes())
        .to_hex()
        .to_string();
    let mut cursor = if let Some(raw) = after {
        invalid(raw.len() <= 16384, "candidate cursor bytes")?;
        serde_json::from_str::<CandidateCursor>(raw)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?
    } else {
        CandidateCursor {
            identity: binding.clone(),
            field: field.into(),
            source: 0,
            ordinal: -1,
            advance_source: true,
        }
    };
    invalid(
        cursor.identity == binding
            && cursor.field == field
            && cursor.source >= 0
            && cursor.ordinal >= -1,
        "candidate cursor belongs to another view",
    )?;
    let mut page = Page {
        rows: Vec::new(),
        next: None,
        scanned: U64(0),
    };
    while page.rows.len() < limit && page.scanned.0 < bounds.scan_rows as u64 {
        check_cancel(cancel)?;
        let before = opaque(&cursor, 16384)?;
        page.scanned.0 += 1;
        if cursor.advance_source {
            let next=sql(db.query_row("SELECT source_id FROM metadata_image_sources WHERE image_id=?1 AND source_id>?2 ORDER BY source_id LIMIT 1",params![image,cursor.source],|r|r.get::<_,i64>(0)).optional())?;
            let Some(source) = next else {
                page.next = None;
                return Ok(page);
            };
            cursor.source = source;
            cursor.ordinal = -1;
            cursor.advance_source = false;
        } else {
            let row=sql(db.query_row("SELECT m.id,m.ordinal,m.observation_id,(a.association='ambiguous' OR o.status!='Complete'),v.semantic_hash FROM metadata_image_sources a JOIN metadata_observations o ON o.id=a.current_observation JOIN metadata_models m ON m.observation_id=a.current_observation LEFT JOIN metadata_values v ON v.model_id=m.id AND v.field=?4 WHERE a.image_id=?1 AND a.source_id=?2 AND m.ordinal>?3 ORDER BY m.ordinal LIMIT 1",params![image,cursor.source,cursor.ordinal,field],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,bool>(3)?,r.get::<_,Option<String>>(4)?))).optional())?;
            if let Some((model, ordinal, observation, ambiguous, hash)) = row {
                cursor.ordinal = ordinal;
                if let Some(semantic_hash) = hash {
                    page.rows.push(Candidate {
                        model: I64(model),
                        source: I64(cursor.source),
                        observation: I64(observation),
                        ordinal: I64(ordinal),
                        ambiguous,
                        semantic_hash,
                        value: required(
                            db,
                            image,
                            TextReference::Candidate {
                                model: I64(model),
                                field: field.into(),
                            },
                        )?,
                    });
                    page.next = Some(opaque(&cursor, 16384)?);
                    if size(&page, budget(bounds).saturating_sub(64)).is_err() {
                        page.rows.pop();
                        page.next = Some(before);
                        if page.rows.is_empty() {
                            return Err(error(
                                ErrorCode::ResourceLimit,
                                "metadata candidate exceeds page bytes",
                            ));
                        }
                        return Ok(page);
                    }
                }
            } else {
                cursor.advance_source = true;
            }
        }
        page.next = Some(opaque(&cursor, 16384)?);
    }
    Ok(page)
}
fn instance_page(
    db: &Connection,
    image: &str,
    after: i64,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<FileInstance>> {
    let mut cursor = after;
    let mut page = Page {
        rows: Vec::new(),
        next: None,
        scanned: U64(0),
    };
    while page.rows.len() < limit && page.scanned.0 < bounds.scan_rows as u64 {
        let row=sql(db.query_row("SELECT id,source_id,EXISTS(SELECT 1 FROM metadata_image_sources a WHERE a.image_id=?1 AND a.source_id=f.source_id) FROM metadata_file_instances f WHERE asset_id=(SELECT asset_id FROM catalog_images WHERE id=?1) AND id>?2 ORDER BY id LIMIT 1",params![image,cursor],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,bool>(2)?))).optional())?;
        let Some((id, source, owned)) = row else {
            page.next = None;
            return Ok(page);
        };
        page.scanned.0 += 1;
        if owned {
            page.rows.push(FileInstance {
                id: I64(id),
                source: I64(source),
                provenance: required(
                    db,
                    image,
                    TextReference::FileInstance {
                        instance: I64(id),
                        field: FileInstanceText::Provenance,
                    },
                )?,
                observed_at: required(
                    db,
                    image,
                    TextReference::FileInstance {
                        instance: I64(id),
                        field: FileInstanceText::ObservedAt,
                    },
                )?,
            });
            page.next = Some(id.to_string());
            if size(&page, budget(bounds).saturating_sub(64)).is_err() {
                page.rows.pop();
                page.next = Some(cursor.to_string());
                if page.rows.is_empty() {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata file-instance exceeds page bytes",
                    ));
                }
                return Ok(page);
            }
        }
        cursor = id;
        page.next = Some(cursor.to_string());
    }
    Ok(page)
}

struct BlobReader<'a> {
    db: &'a Connection,
    hash: &'a str,
    offset: usize,
    length: usize,
    buffer: Vec<u8>,
    consumed: usize,
    cancel: &'a Cancellation,
}
impl Read for BlobReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_canceled() {
            return Err(std::io::Error::other("metadata inspection canceled"));
        }
        if out.is_empty() {
            return Ok(0);
        }
        if self.consumed == self.buffer.len() {
            if self.offset == self.length {
                return Ok(0);
            }
            self.buffer = self
                .db
                .query_row(
                    "SELECT substr(compressed,?2,?3) FROM metadata_blobs WHERE hash=?1",
                    params![
                        self.hash,
                        self.offset as i64 + 1,
                        CHUNK.min(self.length - self.offset) as i64
                    ],
                    |r| r.get(0),
                )
                .map_err(std::io::Error::other)?;
            if self.buffer.is_empty() {
                return Err(std::io::Error::other("retained blob shortened"));
            }
            self.offset += self.buffer.len();
            self.consumed = 0;
        }
        let count = out.len().min(self.buffer.len() - self.consumed);
        out[..count].copy_from_slice(&self.buffer[self.consumed..self.consumed + count]);
        self.consumed += count;
        Ok(count)
    }
}
fn chunk_bounds(offset: u64, length: u32, bounds: &Limits) -> Result<usize> {
    invalid(
        length > 0 && length as usize <= CHUNK && offset <= i64::MAX as u64,
        "metadata chunk bounds",
    )?;
    invalid(
        length as usize * 4 + 512 <= bounds.reply_bytes
            && length as usize * 4 + 512 <= bounds.page_bytes,
        "metadata chunk exceeds transport bounds",
    )?;
    Ok(length as usize)
}
fn chunk(
    bytes: Vec<u8>,
    offset: u64,
    total: u64,
    blake3: Option<String>,
    verified: bool,
    inspected: u64,
) -> Chunk {
    let end = offset + bytes.len() as u64;
    Chunk {
        bytes,
        offset: U64(offset),
        total: U64(total),
        next: (end < total).then_some(U64(end)),
        blake3,
        verified,
        inspected_bytes: U64(inspected),
    }
}
fn blob_chunk(
    db: &Connection,
    image: &str,
    source: &BlobSource,
    offset: u64,
    length: usize,
    cancel: &Cancellation,
) -> Result<Chunk> {
    let hash = match source {
        BlobSource::Packet {
            observation,
            ordinal,
        } => {
            invalid(observation.0 > 0 && ordinal.0 >= 0, "packet identity")?;
            sql(db.query_row("SELECT p.blob_hash FROM metadata_packets p JOIN metadata_image_observations h ON h.observation_id=p.observation_id WHERE h.image_id=?1 AND p.observation_id=?2 AND p.ordinal=?3",params![image,observation.0,ordinal.0],|r|r.get::<_,String>(0)))?
        }
        BlobSource::Model { id } => {
            invalid(id.0 > 0, "model identity")?;
            sql(db.query_row("SELECT m.blob_hash FROM metadata_models m JOIN metadata_image_observations h ON h.observation_id=m.observation_id WHERE h.image_id=?1 AND m.id=?2",params![image,id.0],|r|r.get::<_,String>(0)))?
        }
    };
    let (raw, compressed) = sql(db.query_row(
        "SELECT raw_length,length(compressed) FROM metadata_blobs WHERE hash=?1",
        [&hash],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ))?;
    invalid(
        (0..=RAW_MAX as i64).contains(&raw) && (0..=(RAW_MAX + 65536) as i64).contains(&compressed),
        "retained blob size ceiling",
    )?;
    invalid(offset <= raw as u64, "packet offset exceeds length")?;
    let reader = BlobReader {
        db,
        hash: &hash,
        offset: 0,
        length: compressed as usize,
        buffer: Vec::new(),
        consumed: 0,
        cancel,
    };
    let mut decoder = flate2::read::ZlibDecoder::new(reader);
    let mut buffer = [0u8; CHUNK];
    let mut position = 0u64;
    let mut digest = blake3::Hasher::new();
    let mut result = Vec::with_capacity(length.min((raw as u64 - offset) as usize));
    loop {
        check_cancel(cancel)?;
        let maximum = CHUNK.min((raw as u64 + 1 - position) as usize);
        let read = decoder.read(&mut buffer[..maximum]).map_err(|e| {
            if cancel.is_canceled() {
                error(ErrorCode::Canceled, "metadata inspection canceled")
            } else {
                native(e.into())
            }
        })?;
        if read == 0 {
            break;
        }
        let end = position + read as u64;
        if end > raw as u64 {
            return Err(error(
                ErrorCode::Native,
                "retained blob exceeds declared raw size",
            ));
        }
        digest.update(&buffer[..read]);
        let first = offset.max(position);
        let last = (offset + length as u64).min(end);
        if first < last {
            result.extend_from_slice(
                &buffer[(first - position) as usize..(last - position) as usize],
            );
        }
        position = end;
    }
    invalid(
        position == raw as u64
            && decoder.total_in() == compressed as u64
            && digest.finalize().to_hex().as_str() == hash,
        "retained metadata checksum mismatch",
    )?;
    Ok(chunk(
        result,
        offset,
        raw as u64,
        Some(hash),
        true,
        position,
    ))
}

fn anchor(raw: &str, key: &VariantKey, bounds: &Limits) -> Result<history::Anchor> {
    invalid(
        raw.len() <= bounds.request_bytes.min(16384),
        "history anchor bytes",
    )?;
    #[derive(Deserialize)]
    struct Selected {
        variant: VariantKey,
    }
    let selected: Selected =
        serde_json::from_str(raw).map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
    invalid(
        selected.variant == *key,
        "history anchor belongs to another selected image",
    )?;
    serde_json::from_str(raw).map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))
}
fn imported_row(row: history::RawRow, bounds: &Limits) -> Result<ImportedRow> {
    Ok(ImportedRow {
        record: I64(row.record),
        source_key_json: opaque(&row.source, budget(bounds))?,
        source_id: row.source_id,
        entity_record: I64(row.entity_record),
        table_record: I64(row.table_record),
        cells_field: row.cells_field,
        cells_json_bytes: U64(row.cells_json_bytes),
        classification: row.classification,
    })
}
fn imported_page(page: history::EvidencePage, bounds: &Limits) -> Result<ImportedPage> {
    Ok(ImportedPage {
        key: page.variant,
        input: page.input,
        anchor_json: opaque(&page.anchor, 16384)?,
        row: imported_row(page.row, bounds)?,
        relations: page
            .relations
            .into_iter()
            .map(|r| {
                Ok(Relation {
                    reference_record: I64(r.reference_record),
                    source_id: r.source_id,
                    field: r.field,
                    target_table: r.target_table,
                    target_key: r.target_key,
                    source: r.source.map(|v| imported_row(v, bounds)).transpose()?,
                    target: r.target.map(|v| imported_row(v, bounds)).transpose()?,
                    compatibility: r.compatibility,
                    anchor_json: r.anchor.map(|v| opaque(&v, 16384)).transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        next: page.next.map(|v| opaque(&v, 16384)).transpose()?,
        coverage_complete: page.coverage_complete,
        keys_complete: page.keys_complete,
        adobe_rendering_equivalent: false,
    })
}
fn history_page(
    catalog: &Catalog,
    key: &VariantKey,
    anchor_json: Option<&str>,
    direction: history::Direction,
    after: Option<&str>,
    bounds: &Limits,
) -> Result<ImportedPage> {
    let after = after
        .map(|s| {
            invalid(s.len() <= 16384, "history cursor bytes")?;
            serde_json::from_str::<history::Cursor>(s)
                .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))
        })
        .transpose()?;
    let a = if let Some(raw) = anchor_json {
        anchor(raw, key, bounds)?
    } else {
        invalid(
            after.is_none(),
            "initial history continuation requires returned anchor",
        )?;
        catalog
            .migration_variant_evidence(key, None, 1)
            .map_err(native)?
            .anchor
    };
    // One relationship per bridge call keeps per-hop association work bounded;
    // the existing opaque continuation exposes the complete retained graph.
    imported_page(
        catalog
            .migration_source_evidence(&a, direction, after.as_ref(), 1)
            .map_err(native)?,
        bounds,
    )
}
fn owned_import_record(
    catalog: &Catalog,
    key: &VariantKey,
    raw: &str,
    role: &RecordRole,
    bounds: &Limits,
) -> Result<(i64, crate::lightroom::migration_source::EvidenceRecord)> {
    let a = anchor(raw, key, bounds)?;
    let page = catalog
        .migration_source_evidence(&a, history::Direction::Outgoing, None, 1)
        .map_err(native)?;
    let id = match role {
        RecordRole::Row => page.row.record,
        RecordRole::Table => page.row.table_record,
        RecordRole::Entity => page.row.entity_record,
    };
    let (input, bytes, compressed, seal) = sql(catalog.db.query_row(
        "SELECT r.input,r.raw_length,length(r.compressed),length(i.seal) FROM migration_retained_records r JOIN migration_retention i ON i.id=r.input WHERE r.sequence=?1 AND r.complete=1",
        [id],
        |r| Ok((r.get::<_, String>(0)?, unsigned(r, 1)?,unsigned(r,2)?,unsigned(r,3)?)),
    ))?;
    invalid(
        input == page.input
            && bytes <= 8 * 1024 * 1024
            && compressed <= 8 * 1024 * 1024 + 32768
            && seal <= 8 * 1024 * 1024,
        "retained record ownership or byte limit",
    )?;
    Ok((id, catalog.migration_lookup_record(id).map_err(native)?))
}
fn field_description(
    name: String,
    field: &crate::lightroom::migration_source::Field,
) -> Result<ImportedField> {
    use crate::lightroom::{migration_source::Field, plan::Cell};
    let (representation, bytes, scalar_json) = match field {
        Field::Bytes(v) => (if v.text { "text" } else { "blob" }, v.bytes, None),
        Field::Inline(Cell::Text(b)) => ("text", b.len() as u64, None),
        Field::Inline(Cell::Blob(b)) => ("blob", b.len() as u64, None),
        Field::Inline(v) => {
            let value = opaque(v, 1024)?;
            ("typed_scalar", value.len() as u64, Some(value))
        }
    };
    Ok(ImportedField {
        name,
        representation: representation.into(),
        bytes: U64(bytes),
        scalar_json,
    })
}
fn import_fields(
    catalog: &Catalog,
    key: &VariantKey,
    raw: &str,
    role: &RecordRole,
    after: &str,
    limit: usize,
    bounds: &Limits,
) -> Result<Page<ImportedField>> {
    text(after, true)?;
    let (_, record) = owned_import_record(catalog, key, raw, role, bounds)?;
    use std::ops::Bound::{Excluded, Unbounded};
    let rows = record
        .fields
        .range::<str, _>((Excluded(after), Unbounded))
        .take(limit)
        .map(|(name, field)| Ok((name.clone(), field_description(name.clone(), field)?)))
        .collect::<Result<Vec<_>>>()?;
    finish_page(rows, limit, bounds)
}
fn import_chunk(
    catalog: &Catalog,
    key: &VariantKey,
    raw: &str,
    role: &RecordRole,
    field: &str,
    range: (u64, usize),
    bounds: &Limits,
) -> Result<Chunk> {
    let (offset, length) = range;
    use crate::lightroom::{migration_source::Field, plan::Cell};
    text(field, false)?;
    let (id, record) = owned_import_record(catalog, key, raw, role, bounds)?;
    let value = record
        .fields
        .get(field)
        .ok_or_else(|| error(ErrorCode::InvalidRequest, "retained field absent"))?;
    if let Field::Bytes(reference) = value {
        let evidence = catalog
            .retained_migration_field(id, field)
            .map_err(native)?;
        invalid(
            evidence.complete && evidence.length == reference.bytes,
            "retained field is incomplete",
        )?;
        let descriptor_bytes: i64 = sql(catalog.db.query_row(
            "SELECT length(descriptor) FROM migration_evidence WHERE id=?1",
            [&evidence.id],
            |r| r.get(0),
        ))?;
        invalid(
            (0..=16384).contains(&descriptor_bytes),
            "retained field descriptor bytes",
        )?;
        let descriptor = catalog
            .migration_evidence_descriptor(&evidence.id)
            .map_err(native)?;
        invalid(
            serde_json::from_slice::<crate::lightroom::migration_source::ByteRef>(&descriptor)
                .map_err(|e| native(e.into()))?
                == *reference,
            "retained field descriptor differs",
        )?;
        invalid(offset <= evidence.length, "retained field offset")?;
        if offset == evidence.length {
            return Ok(chunk(Vec::new(), offset, evidence.length, None, true, 0));
        }
        let start:u64=sql(catalog.db.query_row("SELECT offset FROM migration_evidence_chunks WHERE evidence=?1 AND offset<=?2 ORDER BY offset DESC LIMIT 1",params![evidence.id,offset as i64],|r|unsigned(r,0)))?;
        let data = catalog
            .migration_evidence_chunk(&evidence.id, start)
            .map_err(native)?;
        let within = usize::try_from(offset - start).map_err(|e| native(e.into()))?;
        invalid(within < data.len(), "retained field chunk gap")?;
        let end = (within + length).min(data.len());
        let bytes = data[within..end].to_vec();
        return Ok(chunk(
            bytes,
            offset,
            evidence.length,
            None,
            true,
            data.len() as u64,
        ));
    }
    let bytes = match value {
        Field::Inline(Cell::Text(b) | Cell::Blob(b)) => b.clone(),
        Field::Inline(v) => opaque(v, 1024)?.into_bytes(),
        Field::Bytes(_) => unreachable!(),
    };
    invalid(offset <= bytes.len() as u64, "retained inline field offset")?;
    let start = offset as usize;
    let end = (start + length).min(bytes.len());
    let hash = blake3::hash(&bytes).to_hex().to_string();
    Ok(chunk(
        bytes[start..end].to_vec(),
        offset,
        bytes.len() as u64,
        Some(hash),
        true,
        bytes.len() as u64,
    ))
}
fn adobe_page(
    catalog: &Catalog,
    key: &VariantKey,
    raw: &str,
    column: &str,
    path: &str,
    page: (u64, usize),
    bounds: &Limits,
) -> Result<AdobePage> {
    let (after, limit) = page;
    text(column, false)?;
    invalid(
        path.len() <= 16384 && after <= 10000,
        "Adobe property cursor/path bounds",
    )?;
    let a = anchor(raw, key, bounds)?;
    let settings = serde_json::from_str::<Vec<crate::lightroom::adobe::Key>>(path)
        .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
    invalid(settings.len() <= 16, "Adobe settings depth")?;
    let evidence = catalog
        .migration_adobe_evidence(&a, column, settings)
        .map_err(native)?;
    let mut result = AdobePage {
        row: imported_row(evidence.row, bounds)?,
        compatibility: evidence.compatibility,
        reason: evidence.reason,
        input_json: None,
        coordinate_space: None,
        properties: Page {
            rows: Vec::new(),
            next: None,
            scanned: U64(0),
        },
        missing: Vec::new(),
        failure: None,
        contribution_json: None,
        adobe_rendering_equivalent: false,
    };
    if let Some(extraction) = evidence.extraction {
        result.input_json = Some(opaque(&extraction.input, budget(bounds))?);
        result.coordinate_space = Some(extraction.coordinate_space);
        result.missing = extraction.missing;
        result.failure = extraction.failure;
        result.contribution_json = Some(opaque(&extraction.contribution, budget(bounds))?);
        let total = extraction.properties.len();
        invalid(after <= total as u64, "Adobe property offset")?;
        let rows = extraction
            .properties
            .into_iter()
            .enumerate()
            .skip(after as usize)
            .take(limit)
            .map(|(index, p)| {
                Ok((
                    (index + 1).to_string(),
                    AdobeProperty {
                        path_json: opaque(&p.path, budget(bounds))?,
                        namespace: p.namespace,
                        name: p.name,
                        lexical: p.lexical,
                        start: U64(p.start as u64),
                        end: U64(p.end as u64),
                        value_json: opaque(&p.value, budget(bounds))?,
                        disposition: p.disposition,
                        reason: p.reason,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        result.properties = finish_page(rows, limit, bounds)?;
        if after + result.properties.rows.len() as u64 == total as u64 {
            result.properties.next = None;
        }
        // Include compatibility/header bytes in admission; never advance over
        // a property removed to fit the transport envelope.
        while size(&result, budget(bounds).saturating_sub(64)).is_err() {
            result.properties.rows.pop();
            if result.properties.rows.is_empty() {
                return Err(error(
                    ErrorCode::ResourceLimit,
                    "Adobe property/header exceeds page bytes; retained source remains available through import_chunk",
                ));
            }
            result.properties.next =
                Some((after + result.properties.rows.len() as u64).to_string());
        }
    }
    Ok(result)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewCursor {
    identity: String,
    query: String,
    position: String,
}
trait CurrentRow {
    fn position(&self) -> String;
}
impl CurrentRow for Field {
    fn position(&self) -> String {
        self.name.clone()
    }
}
impl CurrentRow for Source {
    fn position(&self) -> String {
        self.id.0.to_string()
    }
}
impl CurrentRow for Observation {
    fn position(&self) -> String {
        self.id.0.to_string()
    }
}
impl CurrentRow for Model {
    fn position(&self) -> String {
        self.ordinal.0.to_string()
    }
}
impl CurrentRow for Packet {
    fn position(&self) -> String {
        self.ordinal.0.to_string()
    }
}
impl CurrentRow for Decision {
    fn position(&self) -> String {
        self.id.0.to_string()
    }
}
impl CurrentRow for FileInstance {
    fn position(&self) -> String {
        self.id.0.to_string()
    }
}
fn view_page<T: Serialize + CurrentRow>(
    db: &Connection,
    identity: &ImageIdentity,
    after: Option<&str>,
    query: &str,
    initial: &str,
    bounds: &Limits,
    read: impl FnOnce(&str, &str) -> Result<Page<T>>,
) -> Result<Page<T>> {
    let image = checked_image(db, identity)?;
    let binding = blake3::hash(opaque(identity, 8192)?.as_bytes())
        .to_hex()
        .to_string();
    let position = if let Some(raw) = after {
        invalid(raw.len() <= 16384, "metadata cursor bytes")?;
        let cursor: ViewCursor = serde_json::from_str(raw)
            .map_err(|e| error(ErrorCode::InvalidRequest, e.to_string()))?;
        invalid(
            cursor.identity == binding && cursor.query == query,
            "metadata cursor belongs to another image or query",
        )?;
        cursor.position
    } else {
        initial.into()
    };
    let mut page = read(&image, &position)?;
    page.next = page
        .next
        .map(|position| {
            opaque(
                &ViewCursor {
                    identity: binding.clone(),
                    query: query.into(),
                    position,
                },
                16384,
            )
        })
        .transpose()?;
    while size(&page, budget(bounds).saturating_sub(64)).is_err() {
        page.rows.pop();
        let row = page.rows.last().ok_or_else(|| {
            error(
                ErrorCode::ResourceLimit,
                "metadata row and cursor exceed page bytes",
            )
        })?;
        page.next = Some(opaque(
            &ViewCursor {
                identity: binding.clone(),
                query: query.into(),
                position: row.position(),
            },
            16384,
        )?);
    }
    Ok(page)
}
fn integer_cursor(raw: &str) -> Result<i64> {
    raw.parse()
        .map_err(|_| error(ErrorCode::InvalidRequest, "metadata cursor position"))
}

pub fn execute(catalog: &mut Catalog, request: Request, bounds: &Limits) -> Result<Response> {
    execute_cancellable(catalog, request, bounds, &Cancellation::default())
}
pub fn execute_cancellable(
    catalog: &mut Catalog,
    request: Request,
    bounds: &Limits,
    cancel: &Cancellation,
) -> Result<Response> {
    invalid(
        (1..=100).contains(&bounds.page_rows)
            && (usize::from(bounds.page_rows)..=4096).contains(&bounds.scan_rows)
            && (1024..=1024 * 1024).contains(&bounds.page_bytes)
            && (1024..=4 * 1024 * 1024).contains(&bounds.request_bytes)
            && (1024..=4 * 1024 * 1024).contains(&bounds.reply_bytes),
        "metadata limits",
    )?;
    size(&request, bounds.request_bytes)?;
    check_cancel(cancel)?;
    if let Request::Resolve {
        key,
        expected_revision,
        field,
        model,
    } = request
    {
        text(&field, false)?;
        text(&key.asset_id, false)?;
        text(&key.variant_id, false)?;
        invalid(
            expected_revision.0 >= 0 && model.0 > 0,
            "metadata resolution identity",
        )?;
        size(
            &Response::Changed {
                revision: I64(i64::MAX),
            },
            bounds.reply_bytes,
        )?;
        return Ok(Response::Changed {
            revision: I64(catalog
                .resolve_metadata_for_image(&key, expected_revision.0, &field, model.0)
                .map_err(native)?),
        });
    }
    // Every ownership check and subsequent retained byte read uses this snapshot.
    let tx = sql(catalog.db.unchecked_transaction())?;
    let response = match request {
        Request::Identity { key } => {
            let image = owned_image(&tx, &key)?;
            Response::Identity(
                catalog_images::identity(&tx, &image)
                    .map_err(native)?
                    .into(),
            )
        }
        Request::Fields {
            identity,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Fields(view_page(
                &tx,
                &identity,
                after.as_deref(),
                "fields",
                "",
                bounds,
                |image, cursor| field_page(&tx, image, cursor, limit, bounds),
            )?)
        }
        Request::Candidates {
            identity,
            field,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            checked_image(&tx, &identity)?;
            Response::Candidates(candidate_page(
                &tx,
                &identity,
                &field,
                after.as_deref(),
                limit,
                bounds,
                cancel,
            )?)
        }
        Request::Sources {
            identity,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Sources(view_page(
                &tx,
                &identity,
                after.as_deref(),
                "sources",
                "0",
                bounds,
                |image, cursor| source_page(&tx, image, integer_cursor(cursor)?, limit, bounds),
            )?)
        }
        Request::Observations {
            identity,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Observations(view_page(
                &tx,
                &identity,
                after.as_deref(),
                "observations",
                "0",
                bounds,
                |image, cursor| {
                    observation_page(&tx, image, integer_cursor(cursor)?, limit, bounds)
                },
            )?)
        }
        Request::Models {
            identity,
            observation,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Models(view_page(
                &tx,
                &identity,
                after.as_deref(),
                &format!("models:{}", observation.0),
                "-1",
                bounds,
                |image, cursor| {
                    model_page(
                        &tx,
                        image,
                        observation.0,
                        integer_cursor(cursor)?,
                        limit,
                        bounds,
                    )
                },
            )?)
        }
        Request::Packets {
            identity,
            observation,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Packets(view_page(
                &tx,
                &identity,
                after.as_deref(),
                &format!("packets:{}", observation.0),
                "-1",
                bounds,
                |image, cursor| {
                    packet_page(
                        &tx,
                        image,
                        observation.0,
                        integer_cursor(cursor)?,
                        limit,
                        bounds,
                    )
                },
            )?)
        }
        Request::Decisions {
            identity,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::Decisions(view_page(
                &tx,
                &identity,
                after.as_deref(),
                "decisions",
                "0",
                bounds,
                |image, cursor| decision_page(&tx, image, integer_cursor(cursor)?, limit, bounds),
            )?)
        }
        Request::FileInstances {
            identity,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            Response::FileInstances(view_page(
                &tx,
                &identity,
                after.as_deref(),
                "file_instances",
                "0",
                bounds,
                |image, cursor| instance_page(&tx, image, integer_cursor(cursor)?, limit, bounds),
            )?)
        }
        Request::TextChunk {
            identity,
            reference,
            offset,
            length,
        } => {
            let length = chunk_bounds(offset.0, length, bounds)?;
            let image = checked_image(&tx, &identity)?;
            let (total, bytes) = text_slice(&tx, &image, &reference, offset.0, length)?
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "text is null"))?;
            let inspected = bytes.len() as u64;
            Response::Chunk(chunk(bytes, offset.0, total, None, false, inspected))
        }
        Request::BlobChunk {
            key,
            source,
            offset,
            length,
        } => {
            let length = chunk_bounds(offset.0, length, bounds)?;
            let image = owned_image(&tx, &key)?;
            Response::Chunk(blob_chunk(&tx, &image, &source, offset.0, length, cancel)?)
        }
        Request::ImportHistory {
            key,
            anchor_json,
            direction,
            after_json,
            limit,
        } => {
            count(limit, bounds)?;
            owned_image(&tx, &key)?;
            Response::ImportHistory(history_page(
                catalog,
                &key,
                anchor_json.as_deref(),
                direction,
                after_json.as_deref(),
                bounds,
            )?)
        }
        Request::ImportFields {
            key,
            anchor_json,
            role,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            owned_image(&tx, &key)?;
            Response::ImportFields(import_fields(
                catalog,
                &key,
                &anchor_json,
                &role,
                &after,
                limit,
                bounds,
            )?)
        }
        Request::ImportChunk {
            key,
            anchor_json,
            role,
            field,
            offset,
            length,
        } => {
            let length = chunk_bounds(offset.0, length, bounds)?;
            owned_image(&tx, &key)?;
            Response::Chunk(import_chunk(
                catalog,
                &key,
                &anchor_json,
                &role,
                &field,
                (offset.0, length),
                bounds,
            )?)
        }
        Request::AdobeProperties {
            key,
            anchor_json,
            column,
            settings_path_json,
            after,
            limit,
        } => {
            let limit = count(limit, bounds)?;
            owned_image(&tx, &key)?;
            Response::AdobeProperties(adobe_page(
                catalog,
                &key,
                &anchor_json,
                &column,
                &settings_path_json,
                (after.0, limit),
                bounds,
            )?)
        }
        Request::Resolve { .. } => unreachable!(),
    };
    check_cancel(cancel)?;
    size(&response, budget(bounds))?;
    tx.commit().map_err(|e| native(e.into()))?;
    Ok(response)
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
