//! Offline, bounded Adobe history navigation over selected immutable custody.
//! Relation anchors prove a path from one imported image; shared files and other
//! images are descriptors only, never shortcuts into another image's history.
use super::{
    images,
    lookup::{Lookup, LookupCursor},
    originals::SourceKey,
    retention,
};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    lightroom::{
        adobe,
        migration_source::{Collection, EvidenceRecord, Field},
        plan::Cell,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_HOPS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Incoming,
    Outgoing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Classification {
    Image,
    CurrentSettings,
    Settings,
    BeforeSettings,
    History,
    Snapshot,
    Metadata,
    Organization,
    Unsupported,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compatibility {
    RetainedOnly,
    Missing,
    Ambiguous,
    Unavailable,
    OutsideImageHistory,
    TraversalLimit,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hop {
    reference: i64,
    direction: Direction,
}
/// Serialized navigation state is untrusted: every hop is re-proven on each call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    variant: VariantKey,
    input: String,
    root: i64,
    hops: Vec<Hop>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    anchor_blake3: String,
    direction: Direction,
    lookup: LookupCursor,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawRow {
    pub record: i64,
    pub source: SourceKey,
    pub source_id: String,
    pub entity_record: i64,
    pub table_record: i64,
    /// Full retained typed cells (including unknown columns) remain accessible
    /// through retained_migration_field/migration_evidence_chunk reads.
    pub cells_field: String,
    pub cells_json_bytes: u64,
    pub classification: Classification,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relation {
    pub reference_record: i64,
    pub source_id: String,
    pub field: String,
    pub target_table: String,
    pub target_key: String,
    pub source: Option<RawRow>,
    pub target: Option<RawRow>,
    pub compatibility: Compatibility,
    pub anchor: Option<Anchor>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidencePage {
    pub variant: VariantKey,
    pub input: String,
    pub anchor: Anchor,
    pub row: RawRow,
    pub relations: Vec<Relation>,
    pub next: Option<Cursor>,
    /// Completeness of retained indexed relationships only, not a claim that
    /// snapshot-only/unknown original tables were projected or interpreted.
    pub coverage_complete: bool,
    pub keys_complete: bool,
    pub adobe_rendering_equivalent: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdobeEvidence {
    pub row: RawRow,
    pub column: String,
    pub compatibility: Compatibility,
    pub reason: String,
    pub extraction: Option<adobe::Extraction>,
}

fn classification(table: &str) -> Classification {
    match table {
        "Adobe_images" => Classification::Image,
        "Adobe_imageDevelopSettings" => Classification::Settings,
        "Adobe_imageDevelopBeforeSettings" => Classification::BeforeSettings,
        "Adobe_libraryImageDevelopHistoryStep" => Classification::History,
        "Adobe_libraryImageDevelopSnapshot" => Classification::Snapshot,
        "Adobe_AdditionalMetadata"
        | "AgHarvestedExifMetadata"
        | "AgHarvestedIptcMetadata"
        | "AgLibraryIPTC"
        | "AgVideoInfo" => Classification::Metadata,
        "AgLibraryKeywordImage" | "AgLibraryCollectionImage" | "AgLibraryCollectionContent" => {
            Classification::Organization
        }
        _ => Classification::Unsupported,
    }
}
fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(
        blake3::hash(&crate::lightroom::bounded_json(value, OUTPUT_BYTES)?)
            .to_hex()
            .to_string(),
    )
}
fn field_size(field: &Field) -> u64 {
    match field {
        Field::Bytes(v) => v.bytes,
        Field::Inline(Cell::Text(v) | Cell::Blob(v)) => v.len() as u64,
        _ => 0,
    }
}
struct View<'a> {
    catalog: &'a Catalog,
    input: String,
    revision: String,
    used: usize,
}
impl View<'_> {
    fn record(&mut self, sequence: i64) -> Result<EvidenceRecord> {
        let (input, revision, bytes): (String,String,i64) = self.catalog.db.query_row(
            "SELECT input,revision,raw_length FROM migration_retained_records WHERE sequence=? AND complete=1", [sequence], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let bytes = usize::try_from(bytes)?;
        ensure!(
            input == self.input && revision == self.revision,
            "history record belongs to another input/capture"
        );
        ensure!(
            bytes <= MAX_BYTES - self.used,
            "history metadata byte bound"
        );
        self.used += bytes;
        self.catalog.migration_lookup_record(sequence)
    }
    fn text(&self, sequence: i64, row: &EvidenceRecord, name: &str) -> Result<String> {
        ensure!(
            matches!(
                row.fields.get(name),
                Some(Field::Inline(Cell::Text(_)))
                    | Some(Field::Bytes(crate::lightroom::migration_source::ByteRef {
                        text: true,
                        ..
                    }))
            ),
            "history key is not retained text"
        );
        let value = retention::field_bytes(&self.catalog.db, sequence, row, name, 4096)?;
        Ok(String::from_utf8(value)?)
    }
    fn unique(&self, query: &Lookup) -> Result<std::result::Result<i64, Compatibility>> {
        let page = self
            .catalog
            .migration_lookup(&self.input, &self.revision, query, None, 2)?;
        if !page.coverage_complete || !page.keys_complete {
            return Ok(Err(Compatibility::Unavailable));
        }
        Ok(match page.records.as_slice() {
            [] => Err(Compatibility::Missing),
            [hit] if page.next.is_none() => Ok(hit.sequence),
            _ => Err(Compatibility::Ambiguous),
        })
    }
    fn row(&mut self, entity: i64) -> Result<RawRow> {
        let e = self.record(entity)?;
        ensure!(
            e.collection == Collection::Entities,
            "history endpoint is not an entity"
        );
        let source_id = self.text(entity, &e, "source_id")?;
        let table = self.text(entity, &e, "table_name")?;
        let sequence = self
            .unique(&Lookup::RowsBySource(source_id.clone()))?
            .map_err(|why| anyhow::anyhow!("history raw source unavailable: {why:?}"))?;
        let row = self.record(sequence)?;
        ensure!(
            row.collection == Collection::Rows
                && self.text(sequence, &row, "source_id")? == source_id
                && self.text(sequence, &row, "table_name")? == table,
            "history entity/raw source differs"
        );
        ensure!(
            matches!(
                row.fields.get("key_json"),
                Some(Field::Inline(Cell::Text(_)))
                    | Some(Field::Bytes(crate::lightroom::migration_source::ByteRef {
                        text: true,
                        ..
                    }))
            ),
            "history source key is not typed text"
        );
        let raw = retention::field_bytes(&self.catalog.db, sequence, &row, "key_json", 65536)?;
        let key: Vec<Cell> = serde_json::from_slice(&raw)?;
        ensure!(
            serde_json::to_vec(&key)? == raw,
            "history raw source key is not canonical"
        );
        let source = SourceKey {
            capture_revision: self.revision.clone(),
            table: table.clone(),
            key,
        };
        source.identity()?;
        let table_record = self
            .unique(&Lookup::TableByName(table.clone()))?
            .map_err(|why| anyhow::anyhow!("history table unavailable: {why:?}"))?;
        let schema = self.record(table_record)?;
        ensure!(
            schema.collection == Collection::Tables
                && self.text(table_record, &schema, "name")? == table,
            "history raw schema differs"
        );
        Ok(RawRow {
            record: sequence,
            source,
            source_id,
            entity_record: entity,
            table_record,
            cells_field: "cells_json".into(),
            cells_json_bytes: field_size(
                row.fields
                    .get("cells_json")
                    .context("history raw cells absent")?,
            ),
            classification: classification(&table),
        })
    }
    fn local_key(&mut self, row: &RawRow) -> Result<String> {
        let entity = self.record(row.entity_record)?;
        self.text(row.entity_record, &entity, "local_key")
    }
    /// Offline equivalent of MigrationSource::resolve: count joined target
    /// matches for the complete source/field/table group, not one edge alone.
    /// A dangling extra edge contributes no match; two matched edges/entities
    /// remain ambiguous, even when an individual edge has a unique endpoint.
    fn group_target(
        &mut self,
        source_id: &str,
        field: &str,
        target_table: &str,
    ) -> Result<std::result::Result<i64, Compatibility>> {
        let page = self.catalog.migration_lookup(
            &self.input,
            &self.revision,
            &Lookup::References {
                source_id: source_id.into(),
                field: Some(field.into()),
                target_table: Some(target_table.into()),
            },
            None,
            100,
        )?;
        if !page.coverage_complete || !page.keys_complete || page.next.is_some() {
            return Ok(Err(Compatibility::Unavailable));
        }
        let mut matched = None;
        for hit in page.records {
            let reference = self.record(hit.sequence)?;
            ensure!(
                reference.collection == Collection::References
                    && self.text(hit.sequence, &reference, "source_id")? == source_id
                    && self.text(hit.sequence, &reference, "field")? == field
                    && self.text(hit.sequence, &reference, "target_table")? == target_table,
                "history reference group differs"
            );
            let key = self.text(hit.sequence, &reference, "target_key")?;
            match self.unique(&Lookup::EntitiesByLocalKey {
                table_name: target_table.into(),
                local_key: key,
            })? {
                Ok(id) => {
                    if matched.replace(id).is_some() {
                        return Ok(Err(Compatibility::Ambiguous));
                    }
                }
                Err(Compatibility::Missing) => (),
                Err(why) => return Ok(Err(why)),
            }
        }
        Ok(matched.ok_or(Compatibility::Missing))
    }
    fn endpoints(&mut self, reference: i64) -> Result<Relation> {
        let r = self.record(reference)?;
        ensure!(
            r.collection == Collection::References,
            "history hop is not a reference"
        );
        let source_id = self.text(reference, &r, "source_id")?;
        let field = self.text(reference, &r, "field")?;
        let target_table = self.text(reference, &r, "target_table")?;
        let target_key = self.text(reference, &r, "target_key")?;
        let from = self.unique(&Lookup::EntitiesBySource(source_id.clone()))?;
        let to = self.unique(&Lookup::EntitiesByLocalKey {
            table_name: target_table.clone(),
            local_key: target_key.clone(),
        })?;
        let group = self.group_target(&source_id, &field, &target_table)?;
        let compatibility = group
            .as_ref()
            .err()
            .or(from.as_ref().err())
            .or(to.as_ref().err())
            .cloned()
            .unwrap_or(Compatibility::RetainedOnly);
        let source = from.ok().map(|id| self.row(id)).transpose()?;
        let target = to.ok().map(|id| self.row(id)).transpose()?;
        Ok(Relation {
            reference_record: reference,
            source_id,
            field,
            target_table,
            target_key,
            source,
            target,
            compatibility,
            anchor: None,
        })
    }
}

type ImportedOrigin = (String, String, String, String, i64, i64, String, String);

fn root<'a>(catalog: &'a Catalog, key: &VariantKey) -> Result<(View<'a>, Anchor, RawRow)> {
    key.validate()?;
    let rows: Vec<ImportedOrigin> = {
        let mut q=catalog.db.prepare("SELECT m.import_source,m.capture_revision,m.source_table,m.source_id,p.retained_record,p.retained_table,p.result,r.input FROM catalog_images i JOIN image_import_map m ON m.image_id=i.id JOIN migration_images p ON p.source_identity=m.source_id AND p.owner=m.import_source AND p.input_digest=m.input_digest JOIN migration_retained_records r ON r.sequence=p.retained_record WHERE i.asset_id=?1 AND i.variant_id=?2 ORDER BY m.import_source,m.capture_revision,m.source_table,m.source_id LIMIT 2")?;
        q.query_map(params![key.asset_id, key.variant_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    ensure!(
        rows.len() == 1,
        "variant needs exactly one retained migration origin"
    );
    let (_, revision, table, identity, record, table_record, result, input) =
        rows.into_iter().next().unwrap();
    ensure!(
        result.len() <= MAX_BYTES && table == "Adobe_images",
        "history image origin bounds/type"
    );
    let result: images::ProjectionResult = serde_json::from_str(&result)?;
    ensure!(
        result.source_identity == identity
            && matches!(&result.outcome,images::Outcome::Image {key:k,..} if k==key),
        "history native mapping differs"
    );
    let mut view = View {
        catalog,
        input: input.clone(),
        revision,
        used: 0,
    };
    let raw = view.record(record)?;
    let source_id = view.text(record, &raw, "source_id")?;
    let entity = view
        .unique(&Lookup::EntitiesBySource(source_id))?
        .map_err(|why| anyhow::anyhow!("history image key unavailable: {why:?}"))?;
    let row = view.row(entity)?;
    ensure!(
        row.record == record
            && row.table_record == table_record
            && row.source.identity()? == identity,
        "history retained image mapping differs"
    );
    let literal = view.local_key(&row)?;
    ensure!(
        view.unique(&Lookup::EntitiesByLocalKey {
            table_name: "Adobe_images".into(),
            local_key: literal
        })? == Ok(entity),
        "history image numeric key ambiguous/unavailable"
    );
    Ok((
        view,
        Anchor {
            variant: key.clone(),
            input,
            root: record,
            hops: Vec::new(),
        },
        row,
    ))
}

fn next_row(current: &RawRow, relation: &Relation, direction: &Direction) -> Option<RawRow> {
    if relation.compatibility != Compatibility::RetainedOnly {
        return None;
    }
    let (from, to) = match direction {
        Direction::Incoming => (relation.target.as_ref()?, relation.source.as_ref()?),
        Direction::Outgoing => (relation.source.as_ref()?, relation.target.as_ref()?),
    };
    if from.record != current.record {
        return None;
    }
    Some(to.clone())
}
fn allowed_step(
    current: &RawRow,
    next: &RawRow,
    relation: &Relation,
    direction: &Direction,
    root: i64,
) -> bool {
    // Exact source schema paths: direct image evidence, its current settings,
    // and settings->before. Shared files/collections/other images are not
    // association bridges into another image's history.
    if current.record == root {
        return match direction {
            Direction::Incoming => {
                relation.target_table == "Adobe_images"
                    && next.source.table != "Adobe_images"
                    && next.source.table != "AgLibraryFile"
            }
            Direction::Outgoing => {
                relation.field == "developSettingsIDCache"
                    && next.source.table == "Adobe_imageDevelopSettings"
            }
        };
    }
    (current.source.table == "Adobe_imageDevelopSettings"
        && next.source.table == "Adobe_imageDevelopBeforeSettings"
        && *direction == Direction::Incoming
        && relation.field == "developSettings")
        || (current.source.table == "Adobe_imageDevelopBeforeSettings"
            && next.source.table == "Adobe_imageDevelopSettings"
            && *direction == Direction::Outgoing
            && relation.field == "developSettings")
}
fn settings_owner(view: &mut View<'_>, next: &RawRow, root: &RawRow) -> Result<bool> {
    if next.source.table != "Adobe_imageDevelopSettings" {
        return Ok(true);
    }
    let query = Lookup::References {
        source_id: next.source_id.clone(),
        field: Some("image".into()),
        target_table: Some("Adobe_images".into()),
    };
    let page = view
        .catalog
        .migration_lookup(&view.input, &view.revision, &query, None, 2)?;
    if !page.coverage_complete
        || !page.keys_complete
        || page.records.len() > 1
        || page.next.is_some()
    {
        return Ok(false);
    }
    let Some(hit) = page.records.first() else {
        return Ok(true);
    };
    let link = view.endpoints(hit.sequence)?;
    Ok(link
        .target
        .as_ref()
        .is_some_and(|target| target.record == root.record))
}
fn resolve<'a>(catalog: &'a Catalog, anchor: &Anchor) -> Result<(View<'a>, RawRow, RawRow)> {
    ensure!(
        anchor.hops.len() <= MAX_HOPS,
        "history path exceeds eight hops"
    );
    let (mut view, expected, mut row) = root(catalog, &anchor.variant)?;
    ensure!(
        anchor.input == expected.input && anchor.root == expected.root,
        "history anchor belongs to another variant/input"
    );
    let root_row = row.clone();
    for hop in &anchor.hops {
        let relation = view.endpoints(hop.reference)?;
        let mut next = next_row(&row, &relation, &hop.direction)
            .context("history reference is not adjacent/unique")?;
        ensure!(
            allowed_step(&row, &next, &relation, &hop.direction, anchor.root)
                && settings_owner(&mut view, &next, &root_row)?,
            "history reference enters another image/shared construct"
        );
        if row.record == anchor.root
            && hop.direction == Direction::Outgoing
            && relation.field == "developSettingsIDCache"
            && next.source.table == "Adobe_imageDevelopSettings"
        {
            next.classification = Classification::CurrentSettings;
        }
        row = next;
    }
    Ok((view, row, root_row))
}

impl Catalog {
    /// Direct source rows referring to this imported image, not sibling images
    /// sharing a file. Outgoing current-develop links use source_evidence below.
    pub fn migration_variant_evidence(
        &self,
        key: &VariantKey,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<EvidencePage> {
        let (_, anchor, _) = root(self, key)?;
        self.migration_source_evidence(&anchor, Direction::Incoming, cursor, limit)
    }
    /// Follow exact retained references in either direction. Missing, ambiguous,
    /// unavailable and non-history targets are reported without association grants.
    pub fn migration_source_evidence(
        &self,
        anchor: &Anchor,
        direction: Direction,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<EvidencePage> {
        ensure!((1..=100).contains(&limit), "history page limit");
        let (mut view, row, root_row) = resolve(self, anchor)?;
        let token = digest(anchor)?;
        if let Some(c) = cursor {
            ensure!(
                c.anchor_blake3 == token && c.direction == direction,
                "history cursor belongs to another anchor/query"
            );
        }
        let query = match direction {
            Direction::Incoming => Lookup::ReferencesByTargetKey {
                target_table: row.source.table.clone(),
                target_key: view.local_key(&row)?,
            },
            Direction::Outgoing => Lookup::References {
                source_id: row.source_id.clone(),
                field: None,
                target_table: None,
            },
        };
        let page = self.migration_lookup(
            &view.input,
            &view.revision,
            &query,
            cursor.map(|c| &c.lookup),
            limit,
        )?;
        let mut relations = Vec::new();
        for hit in &page.records {
            let mut relation = view.endpoints(hit.sequence)?;
            if let Some(mut next) = next_row(&row, &relation, &direction) {
                if !allowed_step(&row, &next, &relation, &direction, anchor.root)
                    || !settings_owner(&mut view, &next, &root_row)?
                {
                    relation.compatibility = Compatibility::OutsideImageHistory;
                } else if anchor.hops.len() == MAX_HOPS {
                    relation.compatibility = Compatibility::TraversalLimit;
                } else {
                    if row.record == anchor.root
                        && direction == Direction::Outgoing
                        && relation.field == "developSettingsIDCache"
                        && next.source.table == "Adobe_imageDevelopSettings"
                    {
                        next.classification = Classification::CurrentSettings;
                    }
                    let mut child = anchor.clone();
                    child.hops.push(Hop {
                        reference: hit.sequence,
                        direction: direction.clone(),
                    });
                    relation.anchor = Some(child);
                    match direction {
                        Direction::Incoming => relation.source = Some(next),
                        Direction::Outgoing => relation.target = Some(next),
                    }
                }
            }
            relations.push(relation);
        }
        let result = EvidencePage {
            variant: anchor.variant.clone(),
            input: view.input,
            anchor: anchor.clone(),
            row,
            relations,
            next: page.next.map(|lookup| Cursor {
                anchor_blake3: token,
                direction,
                lookup,
            }),
            coverage_complete: page.coverage_complete,
            keys_complete: page.keys_complete,
            adobe_rendering_equivalent: false,
        };
        crate::lightroom::bounded_json(&result, OUTPUT_BYTES)?;
        Ok(result)
    }
    /// Interpret one explicitly named typed text field, offline. Never applies a
    /// recipe or upgrades historical evidence into a current observation.
    pub fn migration_adobe_evidence(
        &self,
        anchor: &Anchor,
        column: &str,
        settings_path: Vec<adobe::Key>,
    ) -> Result<AdobeEvidence> {
        ensure!(
            !column.is_empty() && column.len() <= 1024 && settings_path.len() <= 16,
            "history extraction locator bounds"
        );
        let (mut view, row, _) = resolve(self, anchor)?;
        let mut result = AdobeEvidence {
            row: row.clone(),
            column: column.into(),
            compatibility: Compatibility::RetainedOnly,
            reason: "retained typed row; no supported text interpretation".into(),
            extraction: None,
        };
        if !matches!(
            row.classification,
            Classification::CurrentSettings
                | Classification::Settings
                | Classification::BeforeSettings
                | Classification::History
                | Classification::Snapshot
        ) {
            return Ok(result);
        }
        let record = view.record(row.record)?;
        let schema = view.record(row.table_record)?;
        // JSON typed cells encode text as hex; a large raw row stays in chunked
        // custody instead of allocating or truncating it for interpretation.
        if row.cells_json_bytes > MAX_BYTES as u64
            || field_size(
                schema
                    .fields
                    .get("columns_json")
                    .context("history column roster absent")?,
            ) > 65536
        {
            result.reason =
                "interpretation row/column byte limit; use complete retained cells_json chunks"
                    .into();
            return Ok(result);
        }
        let columns: Vec<String> = serde_json::from_slice(&retention::field_bytes(
            &self.db,
            row.table_record,
            &schema,
            "columns_json",
            65536,
        )?)?;
        let cells: Vec<Cell> = serde_json::from_slice(&retention::field_bytes(
            &self.db,
            row.record,
            &record,
            "cells_json",
            MAX_BYTES,
        )?)?;
        ensure!(
            columns.len() == cells.len() && columns.len() <= 4096,
            "history column/cell roster differs"
        );
        let positions: Vec<_> = columns
            .iter()
            .enumerate()
            .filter(|(_, v)| v.as_str() == column)
            .map(|(i, _)| i)
            .collect();
        ensure!(
            positions.len() == 1,
            "history text column missing/ambiguous"
        );
        let Cell::Text(bytes) = &cells[positions[0]] else {
            result.reason = "selected retained field is not typed text".into();
            return Ok(result);
        };
        if bytes.len() > TEXT_BYTES {
            result.reason = "Adobe 4 MiB input limit; complete typed text remains retained".into();
            return Ok(result);
        }
        let association = if matches!(
            row.classification,
            Classification::BeforeSettings | Classification::History | Classification::Snapshot
        ) {
            adobe::Association::Historical
        } else {
            adobe::Association::Unresolved
        };
        let input = adobe::Input {
            source_id: row.source_id.clone(),
            revision: row.source.capture_revision.clone(),
            locator: format!("retained:{}:cells_json:column:{}", row.record, column),
            payload_blake3: blake3::hash(bytes).to_hex().to_string(),
            payload_bytes: bytes.len() as u64,
            format: adobe::Format::CatalogData,
            source_kind: adobe::SourceKind::Unknown,
            association,
            settings_path,
            as_shot_available: false,
        };
        result.extraction = Some(adobe::extract(bytes, input, adobe::Limits::default())?);
        result.reason="offline Adobe syntax/compatibility extraction only; historical/unresolved recipes never applied".into();
        crate::lightroom::bounded_json(&result, OUTPUT_BYTES)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog_migration::{
            images::{Decision, Outcome, Projection, Role},
            organization::{Link, SourceRecord},
            originals::{OriginalDecision, OriginalRequest},
        },
        lightroom::migration_source::tests::Fixture,
        storage_volume::NativePath,
    };
    use std::collections::BTreeMap;
    struct Test {
        _temp: tempfile::TempDir,
        catalog: Catalog,
        keys: Vec<VariantKey>,
        input: String,
        excluded: String,
    }
    impl Test {
        fn new(duplicate: bool, huge: bool) -> Result<Self> {
            Self::with_case(duplicate, huge, "")
        }
        fn with_case(duplicate: bool, huge: bool, case: &str) -> Result<Self> {
            let mut f = Fixture::new();
            let revision = f.revision().to_owned();
            let excluded = f.seal.excluded_revisions[0].clone();
            let approval = b"synthetic history import approval";
            f.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
            f.edit(|db| {
                for (name,columns) in [
                    ("AgLibraryFile",vec!["id_local"]),
                    ("Adobe_images",vec!["id_local","rootFile","masterImage","developSettingsIDCache"]),
                    ("Adobe_imageDevelopSettings",vec!["id_local","image","text"]),
                    ("Adobe_libraryImageDevelopHistoryStep",vec!["id_local","image","text"]),
                    ("Adobe_libraryImageDevelopSnapshot",vec!["id_local","image","text"]),
                    ("Adobe_imageDevelopBeforeSettings",vec!["id_local","developSettings","beforeText"]),
                    ("UnknownHistory",vec!["id_local","image","opaque"]),
                ] {
                    db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,?3,'[]','{}','fixture',1,1,'complete')",params![revision,name,serde_json::to_string(&columns).unwrap()]).unwrap();
                }
                let payload=Cell::Text(b"{ProcessVersion='11.0',Exposure2012=1,UnknownPlugin={opaque='keep'}}".to_vec());
                let mut rows=vec![(10,"AgLibraryFile",vec![Cell::Integer(10)])];
                for image in [20,21,22] {
                    rows.push((image,"Adobe_images",vec![Cell::Integer(image),Cell::Integer(10),if image==21 {Cell::Integer(20)} else {Cell::Null},Cell::RealBits(((image+10) as f64).to_bits())]));
                    rows.push((image+10,"Adobe_imageDevelopSettings",vec![Cell::Integer(image+10),Cell::Integer(image),payload.clone()]));
                    rows.push((image+20,"Adobe_libraryImageDevelopHistoryStep",vec![Cell::Integer(image+20),Cell::Integer(image),payload.clone()]));
                    rows.push((image+30,"Adobe_libraryImageDevelopSnapshot",vec![Cell::Integer(image+30),Cell::Integer(image),payload.clone()]));
                    rows.push((image+40,"Adobe_imageDevelopBeforeSettings",vec![Cell::Integer(image+40),Cell::RealBits(((image+10) as f64).to_bits()),payload.clone()]));
                }
                rows.push((80,"UnknownHistory",vec![Cell::Integer(80),Cell::Integer(20),Cell::Blob(vec![0,255,7])]));
                if huge {rows.push((70,"Adobe_libraryImageDevelopHistoryStep",vec![Cell::Integer(70),Cell::Integer(20),Cell::Text(vec![b'x';9*1024*1024])]));}
                if duplicate {rows.push((300,"Adobe_imageDevelopSettings",vec![Cell::RealBits(30.0f64.to_bits()),Cell::Integer(20),payload]));}
                for (id,table,cells) in rows {
                    let source=format!("h-{id}");
                    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,?5)",params![revision,source,table,serde_json::to_string(&vec![Cell::Integer(id)]).unwrap(),serde_json::to_string(&cells).unwrap()]).unwrap();
                    db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,source,table,serde_json::to_string(&Cell::Integer(if id==300 {30}else{id})).unwrap()]).unwrap();
                    let mut refs=Vec::new();
                    if table=="Adobe_images" {
                        refs.push(("rootFile","AgLibraryFile",10));refs.push(("developSettingsIDCache","Adobe_imageDevelopSettings",id+10));
                        if id==21 {refs.push(("masterImage","Adobe_images",20));}
                    } else if table=="Adobe_imageDevelopBeforeSettings" {refs.push(("developSettings","Adobe_imageDevelopSettings",id-30));}
                    else if table!="AgLibraryFile" {let image=if id==300 || id==70 || id==80 {20}else if id>=50 {id-30}else if id>=40 {id-20}else{id-10};refs.push(("image","Adobe_images",image));}
                    for (field,target,key) in refs {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,?3,?4,?5)",params![revision,source,field,target,serde_json::to_string(&Cell::Integer(key)).unwrap()]).unwrap();
                    }
                }
            });
            if !case.is_empty() {
                f.edit(|db| {
                    let (owner,field,table,keys)=match case {
                        "incoming"=>("h-40","image","Adobe_images",vec![21]),
                        "outgoing"=>("h-20","developSettingsIDCache","Adobe_imageDevelopSettings",vec![31]),
                        "dangling"=>("h-20","developSettingsIDCache","Adobe_imageDevelopSettings",vec![999]),
                        "overlimit"=>("h-20","developSettingsIDCache","Adobe_imageDevelopSettings",(1000..1100).collect()),
                        _=>unreachable!(),
                    };
                    for key in keys {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,?3,?4,?5)",params![revision,owner,field,table,serde_json::to_string(&Cell::Integer(key)).unwrap()]).unwrap();
                    }
                });
            }
            let source = f.open();
            if !case.is_empty() {
                use crate::lightroom::migration_source::Resolution;
                let (owner, field, table) = if case == "incoming" {
                    ("h-40", "image", "Adobe_images")
                } else {
                    (
                        "h-20",
                        "developSettingsIDCache",
                        "Adobe_imageDevelopSettings",
                    )
                };
                let actual = source.resolve(&revision, owner, field, table)?;
                if matches!(case, "incoming" | "outgoing") {
                    assert_eq!(actual, Resolution::Ambiguous);
                } else {
                    assert_eq!(actual, Resolution::Unique("h-30".into()));
                }
            }
            let temp = tempfile::tempdir()?;
            let mut catalog = Catalog::open(temp.path().join("catalog"))?;
            catalog.begin_migration_retention(&source, approval)?;
            for _ in 0..1000 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "fixture retention incomplete"
            );
            let get = |c| -> Result<Vec<(i64, EvidenceRecord)>> {
                let mut records = Vec::new();
                let mut after = 0;
                loop {
                    let page = catalog.retained_migration_records(
                        source.binding_blake3(),
                        &revision,
                        c,
                        after,
                        100,
                    )?;
                    let Some(last) = page.last() else { break };
                    after = last.0;
                    records.extend(page);
                    ensure!(records.len() <= 1000, "fixture record bound");
                }
                Ok(records)
            };
            let mut rows = BTreeMap::new();
            for (n, r) in get(Collection::Rows)? {
                if !r.fields["source_id"].text()?.starts_with("h-") {
                    continue;
                }
                let key: Vec<Cell> = serde_json::from_str(r.fields["key_json"].text()?)?;
                let Cell::Integer(id) = key[0] else {
                    unreachable!()
                };
                rows.insert(
                    id,
                    SourceRecord {
                        retained_record: n,
                        source: SourceKey {
                            capture_revision: revision.clone(),
                            table: r.fields["table_name"].text()?.into(),
                            key,
                        },
                    },
                );
            }
            let tables: BTreeMap<String, i64> = get(Collection::Tables)?
                .into_iter()
                .map(|(n, r)| Ok((r.fields["name"].text()?.into(), n)))
                .collect::<Result<_>>()?;
            let entities: BTreeMap<String, i64> = get(Collection::Entities)?
                .into_iter()
                .map(|(n, r)| Ok((r.fields["source_id"].text()?.into(), n)))
                .collect::<Result<_>>()?;
            let references: BTreeMap<(String, String), i64> = get(Collection::References)?
                .into_iter()
                .map(|(n, r)| {
                    Ok((
                        (
                            r.fields["source_id"].text()?.into(),
                            r.fields["field"].text()?.into(),
                        ),
                        n,
                    ))
                })
                .collect::<Result<_>>()?;
            let link = |from: i64, field: &str, to: i64| Link {
                reference_record: references[&(format!("h-{from}"), field.into())],
                target_entity_record: entities[&format!("h-{to}")],
                field: field.into(),
                target: rows[&to].clone(),
            };
            catalog.register_migration_original(&OriginalRequest {
                import_source: "fixture".into(),
                source: rows[&10].source.clone(),
                retained_record: rows[&10].retained_record,
                decision: OriginalDecision::Create {
                    path: NativePath::from_path(&temp.path().join("offline.CR2")),
                },
            })?;
            let mut keys = Vec::new();
            for id in [20, 21, 22] {
                let result = catalog.project_migration_image(
                    Some(&source),
                    &Projection {
                        origin: rows[&id].clone(),
                        retained_table: tables["Adobe_images"],
                        import_source: "fixture".into(),
                        decision: Decision::Register {
                            file: Box::new(link(id, "rootFile", 10)),
                            role: if id == 21 {
                                Role::Virtual {
                                    master: link(id, "masterImage", 20),
                                }
                            } else {
                                Role::Master
                            },
                            label: format!("history-{id}"),
                        },
                    },
                )?;
                let Outcome::Image { key, .. } = result.outcome else {
                    unreachable!()
                };
                keys.push(key);
            }
            let input = source.binding_blake3().to_owned();
            drop(source);
            drop(f);
            Ok(Self {
                _temp: temp,
                catalog,
                keys,
                input,
                excluded,
            })
        }
    }
    #[test]
    fn offline_variant_history_is_paged_distinct_and_preserves_unknowns() -> Result<()> {
        let t = Test::new(false, false)?;
        for (index, key) in t.keys.iter().enumerate() {
            let mut cursor = None;
            let mut seen = Vec::new();
            loop {
                let p = t
                    .catalog
                    .migration_variant_evidence(key, cursor.as_ref(), 1)?;
                assert!(p.coverage_complete && p.keys_complete);
                for r in p.relations {
                    if let Some(raw) = r.source
                        && raw.source.table != "Adobe_images"
                    {
                        seen.push(raw.source_id);
                    }
                }
                cursor = p.next;
                if cursor.is_none() {
                    break;
                }
            }
            assert!(seen.contains(&format!("h-{}", 40 + index)));
            assert!(!seen.contains(&format!("h-{}", 40 + (index + 1) % 3)));
        }
        let p = t
            .catalog
            .migration_variant_evidence(&t.keys[0], None, 100)?;
        let history = p
            .relations
            .iter()
            .find(|r| r.source.as_ref().is_some_and(|s| s.source_id == "h-40"))
            .unwrap()
            .anchor
            .as_ref()
            .unwrap();
        let result = t
            .catalog
            .migration_adobe_evidence(history, "text", vec![])?;
        let extraction = result.extraction.unwrap();
        assert_eq!(extraction.input.association, adobe::Association::Historical);
        assert!(serde_json::to_string(&extraction)?.contains("UnknownPlugin"));
        assert!(!extraction.adobe_rendering_equivalent);
        let unknown = p
            .relations
            .iter()
            .find(|r| r.source.as_ref().is_some_and(|s| s.source_id == "h-80"))
            .unwrap();
        assert_eq!(
            unknown.source.as_ref().unwrap().classification,
            Classification::Unsupported
        );
        assert!(
            t.catalog
                .migration_adobe_evidence(unknown.anchor.as_ref().unwrap(), "opaque", vec![])?
                .extraction
                .is_none()
        );
        Ok(())
    }
    #[test]
    fn current_before_snapshot_surface_and_cross_variant_cursors() -> Result<()> {
        let t = Test::new(false, false)?;
        let p = t.catalog.migration_variant_evidence(&t.keys[0], None, 1)?;
        assert!(
            t.catalog
                .migration_variant_evidence(&t.keys[1], p.next.as_ref(), 1)
                .is_err()
        );
        let out = t
            .catalog
            .migration_source_evidence(&p.anchor, Direction::Outgoing, None, 100)?;
        assert!(
            out.relations
                .iter()
                .find(|r| r.field == "rootFile")
                .unwrap()
                .anchor
                .is_none()
        );
        let current = out
            .relations
            .iter()
            .find(|r| r.field == "developSettingsIDCache")
            .unwrap();
        assert_eq!(
            current.target.as_ref().unwrap().classification,
            Classification::CurrentSettings
        );
        let extraction = t
            .catalog
            .migration_adobe_evidence(current.anchor.as_ref().unwrap(), "text", vec![])?
            .extraction
            .unwrap();
        assert_eq!(extraction.input.association, adobe::Association::Unresolved);
        let before = t.catalog.migration_source_evidence(
            current.anchor.as_ref().unwrap(),
            Direction::Incoming,
            None,
            100,
        )?;
        let before = before
            .relations
            .iter()
            .find(|r| r.field == "developSettings")
            .unwrap()
            .anchor
            .as_ref()
            .unwrap();
        let result = t
            .catalog
            .migration_adobe_evidence(before, "beforeText", vec![])?;
        assert_eq!(result.row.classification, Classification::BeforeSettings);
        assert!(result.extraction.is_some());
        let mut forged = before.clone();
        forged.variant = t.keys[1].clone();
        assert!(
            t.catalog
                .migration_source_evidence(&forged, Direction::Incoming, None, 1)
                .is_err()
        );
        forged = before.clone();
        forged.input = t.excluded;
        assert!(
            t.catalog
                .migration_source_evidence(&forged, Direction::Incoming, None, 1)
                .is_err()
        );
        let mut long = before.clone();
        while long.hops.len() <= MAX_HOPS {
            long.hops.push(long.hops[0].clone());
        }
        assert!(
            t.catalog
                .migration_source_evidence(&long, Direction::Incoming, None, 1)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn numeric_duplicate_target_does_not_authorize_arbitrary_settings() -> Result<()> {
        let t = Test::new(true, false)?;
        let p = t
            .catalog
            .migration_variant_evidence(&t.keys[0], None, 100)?;
        let out = t
            .catalog
            .migration_source_evidence(&p.anchor, Direction::Outgoing, None, 100)?;
        let settings = out
            .relations
            .iter()
            .find(|r| r.field == "developSettingsIDCache")
            .unwrap();
        assert_eq!(settings.compatibility, Compatibility::Ambiguous);
        assert!(settings.target.is_none() && settings.anchor.is_none());
        Ok(())
    }
    #[test]
    fn oversized_text_keeps_complete_offline_chunk_locator() -> Result<()> {
        let t = Test::new(false, true)?;
        let p = t
            .catalog
            .migration_variant_evidence(&t.keys[0], None, 100)?;
        let huge = p
            .relations
            .iter()
            .find(|r| r.source.as_ref().is_some_and(|s| s.source_id == "h-70"))
            .unwrap();
        let result =
            t.catalog
                .migration_adobe_evidence(huge.anchor.as_ref().unwrap(), "text", vec![])?;
        assert!(result.extraction.is_none() && result.row.cells_json_bytes > 16 * 1024 * 1024);
        let evidence = t
            .catalog
            .retained_migration_field(result.row.record, "cells_json")?;
        assert!(serde_json::to_string(&evidence)?.contains("complete"));
        assert_eq!(p.input, t.input);
        Ok(())
    }
    #[test]
    fn incoming_conflicting_history_owners_keep_rows_without_variant_anchors() -> Result<()> {
        let t = Test::with_case(false, false, "incoming")?;
        for key in &t.keys[..2] {
            let page = t.catalog.migration_variant_evidence(key, None, 100)?;
            let relation = page
                .relations
                .iter()
                .find(|r| r.source_id == "h-40")
                .unwrap();
            assert_eq!(relation.compatibility, Compatibility::Ambiguous);
            assert!(relation.anchor.is_none());
            assert_eq!(
                relation.source.as_ref().unwrap().classification,
                Classification::History
            );
            assert_eq!(relation.target.as_ref().unwrap().record, page.row.record);
            // Serialized private fields are not permission: re-prove the group.
            let mut forged = page.anchor.clone();
            forged.hops.push(Hop {
                reference: relation.reference_record,
                direction: Direction::Incoming,
            });
            assert!(
                t.catalog
                    .migration_source_evidence(&forged, Direction::Outgoing, None, 1)
                    .is_err()
            );
            assert!(
                t.catalog
                    .migration_adobe_evidence(&forged, "text", vec![])
                    .is_err()
            );
            let row = relation.source.as_ref().unwrap();
            let retained = t.catalog.migration_lookup_record(row.record)?;
            let cells = retention::field_bytes(
                &t.catalog.db,
                row.record,
                &retained,
                "cells_json",
                MAX_BYTES,
            )?;
            assert_eq!(cells.len() as u64, row.cells_json_bytes);
            assert!(!serde_json::from_slice::<Vec<Cell>>(&cells)?.is_empty());
        }
        Ok(())
    }
    #[test]
    fn outgoing_conflicting_current_groups_never_label_or_mint_current_settings() -> Result<()> {
        let t = Test::with_case(false, false, "outgoing")?;
        let base = t.catalog.migration_variant_evidence(&t.keys[0], None, 1)?;
        let out =
            t.catalog
                .migration_source_evidence(&base.anchor, Direction::Outgoing, None, 100)?;
        let targets = out
            .relations
            .iter()
            .filter(|r| r.field == "developSettingsIDCache")
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 2);
        for relation in targets {
            assert_eq!(relation.compatibility, Compatibility::Ambiguous);
            assert!(relation.anchor.is_none());
            assert_eq!(
                relation.target.as_ref().unwrap().classification,
                Classification::Settings
            );
            let mut forged = base.anchor.clone();
            forged.hops.push(Hop {
                reference: relation.reference_record,
                direction: Direction::Outgoing,
            });
            assert!(
                t.catalog
                    .migration_source_evidence(&forged, Direction::Incoming, None, 1)
                    .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn dangling_extra_reference_keeps_unique_join_semantics_but_excess_group_is_unavailable()
    -> Result<()> {
        let t = Test::with_case(false, false, "dangling")?;
        let base = t.catalog.migration_variant_evidence(&t.keys[0], None, 1)?;
        let out =
            t.catalog
                .migration_source_evidence(&base.anchor, Direction::Outgoing, None, 100)?;
        let targets = out
            .relations
            .iter()
            .filter(|r| r.field == "developSettingsIDCache")
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 2);
        let existing = targets.iter().find(|r| r.target.is_some()).unwrap();
        assert_eq!(existing.compatibility, Compatibility::RetainedOnly);
        assert!(existing.anchor.is_some());
        let missing = targets.iter().find(|r| r.target.is_none()).unwrap();
        assert_eq!(missing.compatibility, Compatibility::Missing);
        assert!(missing.anchor.is_none());
        let t = Test::with_case(false, false, "overlimit")?;
        let base = t.catalog.migration_variant_evidence(&t.keys[0], None, 1)?;
        let out =
            t.catalog
                .migration_source_evidence(&base.anchor, Direction::Outgoing, None, 100)?;
        assert!(out.next.is_some());
        assert!(
            out.relations
                .iter()
                .filter(|r| r.field == "developSettingsIDCache")
                .all(|r| r.compatibility == Compatibility::Unavailable && r.anchor.is_none())
        );
        let last = t.catalog.migration_source_evidence(
            &base.anchor,
            Direction::Outgoing,
            out.next.as_ref(),
            100,
        )?;
        let existing = last
            .relations
            .iter()
            .find(|r| r.field == "developSettingsIDCache" && r.target.is_some())
            .unwrap();
        assert_eq!(existing.compatibility, Compatibility::Unavailable);
        assert!(existing.anchor.is_none());
        Ok(())
    }
}
