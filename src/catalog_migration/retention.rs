//! Incremental custody of selected inspection evidence, before native projection.
//! One step retains up to 64 small records or one evidence chunk. It never opens an
//! original image, capture artifact path, or excluded capture.
use super::evidence::{self, PreparedChunk};
use crate::lightroom::migration_source::MigrationRead;
use crate::{
    Catalog,
    catalog_writer::Priority,
    lightroom::migration_source::{Collection, Cursor, EvidenceRecord, Field, MigrationSource},
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

const RECORD_LIMIT: usize = 8 * 1024 * 1024;
const COLLECTIONS: &[Collection] = &[
    Collection::Captures,
    Collection::SchemaObjects,
    Collection::Tables,
    Collection::Rows,
    Collection::Entities,
    Collection::References,
    Collection::Paths,
    Collection::Packets,
    Collection::MetadataFacts,
    Collection::Issues,
];

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_retention(
        id TEXT PRIMARY KEY, seal BLOB NOT NULL, approval BLOB NOT NULL,
        capture_index INTEGER NOT NULL DEFAULT 0, collection_index INTEGER NOT NULL DEFAULT 0,
        cursor TEXT, records INTEGER NOT NULL DEFAULT 0,
        complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN(0,1)));
     CREATE TABLE IF NOT EXISTS migration_retained_records(
        sequence INTEGER PRIMARY KEY, input TEXT NOT NULL REFERENCES migration_retention(id),
        revision TEXT NOT NULL, collection INTEGER NOT NULL, source_rowid INTEGER NOT NULL,
        compressed BLOB NOT NULL, raw_length INTEGER NOT NULL CHECK(raw_length<=8388608),
        digest TEXT NOT NULL, next_cursor TEXT NOT NULL,
        complete INTEGER NOT NULL CHECK(complete IN(0,1)),
        UNIQUE(input,revision,collection,source_rowid));
     CREATE INDEX IF NOT EXISTS migration_retained_pending ON migration_retained_records(input,sequence) WHERE complete=0;
     CREATE INDEX IF NOT EXISTS migration_retained_page ON migration_retained_records(input,revision,collection,sequence);
     CREATE TABLE IF NOT EXISTS migration_retained_fields(
        record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        field TEXT NOT NULL, evidence TEXT NOT NULL REFERENCES migration_evidence(id),
        PRIMARY KEY(record,field));")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom::migration_source::{ReadLimits, tests::Fixture};

    #[test]
    fn opening_retained_field_guards_descriptor_and_identity_before_copy() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.pragma_update(None, "foreign_keys", true)?;
        install(&db)?;
        evidence::install(&db)?;
        let reference = crate::lightroom::migration_source::ByteRef {
            seal: "a".repeat(64),
            revision: "b".repeat(64),
            collection: Collection::Captures,
            rowid: 1,
            field: "manifest".into(),
            bytes: 3,
            text: true,
        };
        // Real parent rows keep the ordinary schema/foreign-key contract on.
        // Only the deliberately corrupt reference is deferred inside a
        // savepoint that is always rolled back before release.
        db.execute(
            "INSERT INTO migration_retention(id,seal,approval) VALUES(?1,x'00',x'00')",
            [&reference.seal],
        )?;
        db.execute("INSERT INTO migration_retained_records(sequence,input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete)
                    VALUES(1,?1,?2,0,1,x'00',0,?3,'',1)", params![reference.seal,reference.revision,"0".repeat(64)])?;
        let id = evidence::begin(&db, &serde_json::to_vec(&reference)?, 3)?.id;
        evidence::append(&db, &id, 0, &PreparedChunk::new(b"yes")?)?;
        db.execute(
            "INSERT INTO migration_retained_fields VALUES(1,'manifest',?1)",
            [&id],
        )?;
        let record = EvidenceRecord {
            revision: reference.revision.clone(),
            collection: Collection::Captures,
            rowid: 1,
            key: vec![],
            fields: [("manifest".into(), Field::Bytes(reference))].into(),
        };
        for (sql, expected) in [
            (
                "UPDATE migration_retained_fields SET evidence=replace(hex(zeroblob(33)),'0','é')",
                "evidence identity type/size",
            ),
            (
                "UPDATE migration_retained_fields SET evidence=zeroblob(64)",
                "evidence identity type/size",
            ),
            (
                "UPDATE migration_evidence SET descriptor=zeroblob(65537)",
                "descriptor type/size",
            ),
            (
                "UPDATE migration_evidence SET descriptor=replace(hex(zeroblob(16385)),'0','é')",
                "descriptor type/size",
            ),
        ] {
            db.execute_batch("SAVEPOINT corrupt; PRAGMA defer_foreign_keys=ON")?;
            db.execute_batch(sql)?;
            let error = field_bytes(&db, 1, &record, "manifest", 3).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{sql}: {error:#}");
            db.execute_batch("ROLLBACK TO corrupt; RELEASE corrupt")?;
            assert_eq!(
                db.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                0
            );
            assert_eq!(field_bytes(&db, 1, &record, "manifest", 3)?, b"yes");
        }
        Ok(())
    }

    #[test]
    fn selected_large_evidence_survives_restart_and_source_disconnect() -> Result<()> {
        let mut fixture = Fixture::with_large_cell(17 * 1024 * 1024);
        let approval = b"approved synthetic selected-only migration test";
        fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
        let source = MigrationSource::open(
            fixture.seal.clone(),
            ReadLimits {
                chunk_bytes: 256 * 1024,
                ..ReadLimits::default()
            },
        )?;
        let id = source.binding_blake3().to_owned();
        let revision = fixture.revision().to_owned();
        let excluded = fixture.seal.excluded_revisions[0].clone();
        let original = source.page(&revision, Collection::Rows, None, 1)?;
        let Field::Bytes(raw) = &original.records[0].fields["cells_json"] else {
            anyhow::bail!("fixture did not exercise oversized source bytes");
        };
        let mut expected = blake3::Hasher::new();
        let mut offset = 0;
        while offset < raw.bytes {
            let bytes = source.read_chunk(raw, offset, source.max_chunk_bytes())?;
            expected.update(&bytes);
            offset += bytes.len() as u64;
        }
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        let mut catalog = Catalog::open(&root)?;
        catalog.begin_migration_retention(&source, approval)?;
        let mut restarted = false;
        for _ in 0..1000 {
            let progress = catalog.step_migration_retention(&source)?;
            if !restarted && catalog.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM migration_evidence WHERE committed>0 AND complete=0)", [], |r|r.get::<_,bool>(0))? {
                drop(catalog);
                catalog = Catalog::open(&root)?;
                catalog.begin_migration_retention(&source, approval)?;
                restarted = true;
            }
            if progress.complete {
                break;
            }
        }
        assert!(restarted);
        let complete = catalog.migration_retention_progress(&id)?;
        assert!(complete.complete);
        assert_eq!(catalog.step_migration_retention(&source)?, complete);
        assert!(
            catalog
                .retained_migration_records(&id, &excluded, Collection::Rows, 0, 100)?
                .is_empty()
        );
        let rows = catalog.retained_migration_records(&id, &revision, Collection::Rows, 0, 100)?;
        assert_eq!(rows.len(), 1);
        let retained = catalog.retained_migration_field(rows[0].0, "cells_json")?;
        assert!(retained.complete);
        assert_eq!(retained.length, raw.bytes);
        drop(source);
        std::fs::rename(&fixture.path, fixture.path.with_extension("disconnected"))?;
        drop(catalog);
        let catalog = Catalog::open(&root)?;
        let mut actual = blake3::Hasher::new();
        let mut offset = 0;
        while offset < retained.length {
            let bytes = catalog.migration_evidence_chunk(&retained.id, offset)?;
            actual.update(&bytes);
            offset += bytes.len() as u64;
        }
        assert_eq!(actual.finalize(), expected.finalize());
        assert!(
            catalog
                .migration_evidence_chunk(&retained.id, offset)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn batches_stop_before_uncustodied_bytes_and_resume_without_skips() -> Result<()> {
        use crate::lightroom::plan::Cell;
        let mut fixture = Fixture::new();
        let revision = fixture.revision().to_owned();
        let approval = b"approved bounded batch synthetic test";
        fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
        fixture.edit(|db| {
            db.execute("DELETE FROM rows WHERE revision=?", [&revision]).unwrap();
            for n in 1..=150i64 {
                let cells = if n == 70 {
                    vec![Cell::Blob(vec![173; 200_000])]
                } else {
                    vec![Cell::Integer(n)]
                };
                db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,'unknown_plugin',?3,?4)",
                    params![revision,format!("row-{n}"),serde_json::to_string(&vec![Cell::Integer(n)]).unwrap(),serde_json::to_string(&cells).unwrap()]).unwrap();
            }
        });
        let source = fixture.open();
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        let mut catalog = Catalog::open(&root)?;
        catalog.begin_migration_retention(&source, approval)?;
        let mut before = catalog.migration_retention_progress(source.binding_blake3())?;
        let mut saw_batch = false;
        let mut saw_pending = false;
        for _ in 0..1000 {
            let after = catalog.step_migration_retention(&source)?;
            let delta = after.records - before.records;
            assert!(delta <= 64);
            saw_batch |= delta == 64;
            let pending: Option<i64> = catalog
                .db
                .query_row(
                    "SELECT sequence FROM migration_retained_records WHERE complete=0",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(sequence) = pending {
                // No later record may be visible while a large predecessor waits.
                assert_eq!(
                    catalog.db.query_row(
                        "SELECT count(*) FROM migration_retained_records WHERE sequence>?",
                        [sequence],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                if COLLECTIONS[before.collection_index] == Collection::Rows {
                    assert_eq!(
                        catalog
                            .retained_migration_records(
                                source.binding_blake3(),
                                &revision,
                                Collection::Rows,
                                0,
                                100
                            )?
                            .len(),
                        69
                    );
                    if !saw_pending {
                        drop(catalog);
                        catalog = Catalog::open(&root)?;
                        catalog.begin_migration_retention(&source, approval)?;
                        saw_pending = true;
                    }
                }
            }
            before = after;
            if before.complete {
                break;
            }
        }
        assert!(before.complete && saw_batch && saw_pending);
        let mut rows = Vec::new();
        let mut cursor = 0;
        loop {
            let page = catalog.retained_migration_records(
                source.binding_blake3(),
                &revision,
                Collection::Rows,
                cursor,
                100,
            )?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().0;
            rows.extend(page);
        }
        assert_eq!(rows.len(), 150);
        for (i, (_, record)) in rows.iter().enumerate() {
            let key = record.fields["key_json"].text()?;
            assert_eq!(
                serde_json::from_str::<Vec<Cell>>(key)?,
                vec![Cell::Integer(i as i64 + 1)]
            );
        }
        assert_eq!(catalog.step_migration_retention(&source)?, before);
        Ok(())
    }

    #[test]
    fn public_evidence_cannot_preseed_or_append_selected_source_custody() -> Result<()> {
        let mut fixture = Fixture::with_large_cell(100_000);
        let approval = b"approved custody authority regression";
        fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
        let source = fixture.open();
        let revision = fixture.revision();
        let page = source.page(revision, Collection::Rows, None, 1)?;
        let Field::Bytes(reference) = &page.records[0].fields["cells_json"] else {
            panic!("large field expected")
        };
        let descriptor = serde_json::to_vec(reference)?;
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let generic = catalog.begin_migration_evidence(&descriptor, reference.bytes)?;
        // Exact descriptor/length plus forged complete bytes used to be mistaken
        // for source custody. These public bytes must occupy a different domain.
        let bogus = vec![b'!'; usize::try_from(reference.bytes)?];
        catalog.append_migration_evidence(&generic.id, 0, &bogus)?;
        catalog.begin_migration_retention(&source, approval)?;
        let mut denied = false;
        for _ in 0..100 {
            let progress = catalog.step_migration_retention(&source)?;
            let owned:Option<String>=catalog.db.query_row("SELECT f.evidence FROM migration_retained_fields f JOIN migration_retained_records r ON r.sequence=f.record WHERE r.complete=0 LIMIT 1",[],|r|r.get(0)).optional()?;
            if let Some(owned) = owned {
                assert_ne!(owned, generic.id);
                let before = catalog.migration_evidence(&owned)?;
                assert!(
                    catalog
                        .append_migration_evidence(&owned, before.committed, b"!")
                        .is_err()
                );
                assert_eq!(catalog.migration_evidence(&owned)?, before);
                denied = true;
            }
            if progress.complete {
                break;
            }
        }
        assert!(
            denied
                && catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete
        );
        let rows = catalog.retained_migration_records(
            source.binding_blake3(),
            revision,
            Collection::Rows,
            0,
            100,
        )?;
        let owned = catalog.retained_migration_field(rows[0].0, "cells_json")?;
        assert_ne!(owned.id, generic.id);
        let actual = catalog.migration_evidence_chunk(&owned.id, 0)?;
        let expected = source.read_chunk(reference, 0, source.max_chunk_bytes())?;
        assert_eq!(actual, expected);
        assert_ne!(actual, bogus);
        Ok(())
    }

    #[test]
    fn invalid_authorization_cannot_create_destination_session() -> Result<()> {
        let fixture = Fixture::new();
        let source = fixture.open();
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        assert!(
            catalog
                .begin_migration_retention(&source, b"different approval")
                .is_err()
        );
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM migration_retention", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionProgress {
    pub input: String,
    pub capture_index: usize,
    pub collection_index: usize,
    pub records: u64,
    /// This means inspection-record custody is complete, not image projection or
    /// raw companion retention. The migration reconciler owns those other stages.
    pub complete: bool,
}

fn progress(db: &Connection, id: &str) -> Result<RetentionProgress> {
    Ok(db.query_row("SELECT capture_index,collection_index,records,complete FROM migration_retention WHERE id=?1", [id],
        |r|Ok(RetentionProgress{input:id.into(),capture_index:evidence::size(r,0)?,collection_index:evidence::size(r,1)?,records:evidence::unsigned(r,2)?,complete:r.get(3)?}))?)
}
fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() <= RECORD_LIMIT, "retained record size limit");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
fn decode(bytes: &[u8], length: usize, digest: &str) -> Result<EvidenceRecord> {
    ensure!(
        length <= RECORD_LIMIT && bytes.len() <= RECORD_LIMIT + 32768,
        "retained record size limit"
    );
    let mut raw = Vec::new();
    let mut decoder = ZlibDecoder::new(bytes);
    (&mut decoder)
        .take(length as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(
        raw.len() == length
            && decoder.total_in() == bytes.len() as u64
            && blake3::hash(&raw).to_hex().as_str() == digest,
        "retained record integrity mismatch"
    );
    Ok(serde_json::from_slice(&raw)?)
}

/// Resolve completed destination evidence to its retained authorized selection.
pub(crate) fn selected_record(db: &Connection, sequence: i64) -> Result<EvidenceRecord> {
    let (input, seal, compressed, length, digest):
        (Option<String>, Option<Vec<u8>>, Option<Vec<u8>>, usize, Option<String>) = db.query_row(
        "SELECT CASE WHEN typeof(i.id)='text' AND length(CAST(i.id AS BLOB))=64 THEN i.id END,
         CASE WHEN typeof(i.seal)='blob' AND length(i.seal)<=?2 THEN i.seal END,
         CASE WHEN typeof(r.compressed)='blob' AND length(r.compressed)<=?3
          AND typeof(r.raw_length)='integer' AND r.raw_length BETWEEN 0 AND ?2 THEN r.compressed END,
         r.raw_length,
         CASE WHEN typeof(r.digest)='text' AND length(CAST(r.digest AS BLOB))=64 THEN r.digest END
         FROM migration_retained_records r JOIN migration_retention i ON i.id=r.input WHERE r.sequence=?1 AND r.complete=1",
        params![sequence, i64::try_from(RECORD_LIMIT)?, i64::try_from(RECORD_LIMIT + 32768)?],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,evidence::size(r,3)?,r.get(4)?)),
    )?;
    let input = input.context("retained input identity type/size limit")?;
    let seal = seal.context("retained seal type/size limit")?;
    let compressed = compressed.context("retained record type/size limit")?;
    let digest = digest.context("retained digest type/size limit")?;
    let seal: crate::lightroom::migration_source::InputSeal = serde_json::from_slice(&seal)?;
    ensure!(
        seal.binding_blake3()? == input,
        "retained seal binding differs"
    );
    let record = decode(&compressed, length, &digest)?;
    ensure!(
        seal.selected.iter().any(|s| s.revision == record.revision)
            && !seal.excluded_revisions.contains(&record.revision),
        "record is not an authorized selected capture"
    );
    Ok(record)
}

pub(crate) fn field_bytes(
    db: &Connection,
    sequence: i64,
    record: &EvidenceRecord,
    name: &str,
    maximum: usize,
) -> Result<Vec<u8>> {
    match record.fields.get(name).context("missing retained field")? {
        Field::Inline(
            crate::lightroom::plan::Cell::Text(bytes) | crate::lightroom::plan::Cell::Blob(bytes),
        ) => {
            ensure!(
                bytes.len() <= maximum,
                "retained field exceeds interpretation limit"
            );
            Ok(bytes.clone())
        }
        Field::Bytes(reference) => {
            ensure!(
                reference.bytes <= maximum as u64,
                "retained field exceeds interpretation limit"
            );
            let id: Option<String> = db.query_row(
                "SELECT CASE WHEN typeof(evidence)='text' AND length(CAST(evidence AS BLOB))=64
                 THEN evidence END FROM migration_retained_fields WHERE record=?1 AND field=?2",
                params![sequence, name],
                |r| r.get(0),
            )?;
            let id = id.context("retained field evidence identity type/size limit")?;
            let descriptor: Option<Vec<u8>> = db.query_row(
                "SELECT CASE WHEN typeof(descriptor)='blob' AND length(descriptor)<=?2
                 THEN descriptor END FROM migration_evidence WHERE id=?1",
                params![id, i64::try_from(evidence::DESCRIPTOR_BYTES)?],
                |r| r.get(0),
            )?;
            let descriptor = descriptor.context("retained field descriptor type/size limit")?;
            ensure!(
                serde_json::from_slice::<crate::lightroom::migration_source::ByteRef>(&descriptor)?
                    == *reference,
                "retained field descriptor differs"
            );
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < reference.bytes {
                let chunk = evidence::read(db, &id, bytes.len() as u64)?;
                ensure!(
                    !chunk.is_empty() && chunk.len() <= maximum - bytes.len(),
                    "retained field length differs"
                );
                bytes.extend(chunk);
            }
            ensure!(
                bytes.len() as u64 == reference.bytes,
                "retained field length differs"
            );
            Ok(bytes)
        }
        _ => anyhow::bail!("retained field is not text or bytes"),
    }
}

impl Catalog {
    /// Opening the source already verified its byte seal and selected family
    /// evidence. The exact external authorization is retained and digest checked.
    pub fn begin_migration_retention(
        &mut self,
        source: &MigrationSource,
        approval: &[u8],
    ) -> Result<RetentionProgress> {
        self.begin_migration_retention_reader(source, approval)
    }
    pub(crate) fn begin_migration_retention_reader(
        &mut self,
        source: &dyn MigrationRead,
        approval: &[u8],
    ) -> Result<RetentionProgress> {
        ensure!(
            approval.len() <= RECORD_LIMIT,
            "selection approval size limit"
        );
        ensure!(
            blake3::hash(approval).to_hex().as_str() == source.seal().approval.document_blake3,
            "selection authorization differs"
        );
        let seal = serde_json::to_vec(source.seal())?;
        ensure!(seal.len() <= RECORD_LIMIT, "source seal size limit");
        let id = source.binding_blake3();
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO migration_retention(id,seal,approval) VALUES(?1,?2,?3)",
            params![id, seal, approval],
        )?;
        let existing: (Vec<u8>, Vec<u8>) = tx.query_row(
            "SELECT seal,approval FROM migration_retention WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            existing.0 == seal && existing.1 == approval,
            "retained selection identity differs"
        );
        let result = progress(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn migration_retention_progress(&self, id: &str) -> Result<RetentionProgress> {
        progress(&self.db, id)
    }

    pub fn step_migration_retention(
        &mut self,
        source: &MigrationSource,
    ) -> Result<RetentionProgress> {
        self.step_migration_retention_reader(source)
    }
    pub(crate) fn step_migration_retention_reader(
        &mut self,
        source: &dyn MigrationRead,
    ) -> Result<RetentionProgress> {
        let id = source.binding_blake3();
        let before = progress(&self.db, id)?;
        if before.complete {
            return Ok(before);
        }
        let pending:Option<(i64,Vec<u8>,usize,String,String)>=self.db.query_row(
            "SELECT sequence,compressed,raw_length,digest,next_cursor FROM migration_retained_records WHERE input=?1 AND complete=0 ORDER BY sequence LIMIT 1",
            [id],|r|Ok((r.get(0)?,r.get(1)?,evidence::size(r,2)?,r.get(3)?,r.get(4)?))).optional()?;
        if let Some((sequence, compressed, length, digest, next)) = pending {
            let record = decode(&compressed, length, &digest)?;
            let field:Option<(String,String,u64)>=self.db.query_row(
                "SELECT f.field,e.id,e.committed FROM migration_retained_fields f JOIN migration_evidence e ON e.id=f.evidence WHERE f.record=?1 AND e.complete=0 ORDER BY f.field LIMIT 1",
                [sequence],|r|Ok((r.get(0)?,r.get(1)?,evidence::unsigned(r,2)?))).optional()?;
            let prepared = if let Some((field, evidence, offset)) = field {
                let Field::Bytes(reference) = record
                    .fields
                    .get(&field)
                    .context("missing retained field descriptor")?
                else {
                    anyhow::bail!("retained field is not external bytes");
                };
                let bytes = source.read_chunk(
                    reference,
                    offset,
                    evidence::CHUNK_BYTES.min(source.max_chunk_bytes()),
                )?;
                Some((evidence, offset, PreparedChunk::new(&bytes)?))
            } else {
                None
            };
            let _permit = self.writers.enter(Priority::Background)?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure!(
                progress(&tx, id)? == before,
                "migration advanced concurrently; retry step"
            );
            if let Some((evidence, offset, chunk)) = &prepared {
                evidence::append_owned(
                    &tx,
                    evidence,
                    *offset,
                    chunk,
                    evidence::Authority::SelectedSource,
                )?;
            }
            let incomplete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM migration_retained_fields f JOIN migration_evidence e ON e.id=f.evidence WHERE f.record=?1 AND e.complete=0)",[sequence],|r|r.get(0))?;
            if !incomplete {
                let changed=tx.execute("UPDATE migration_retained_records SET complete=1 WHERE sequence=?1 AND complete=0",[sequence])?;
                if changed == 1 {
                    tx.execute(
                        "UPDATE migration_retention SET cursor=?2,records=records+1 WHERE id=?1",
                        params![id, next],
                    )?;
                }
            }
            let result = progress(&tx, id)?;
            tx.commit()?;
            return Ok(result);
        }
        let revision = &source
            .seal()
            .selected
            .get(before.capture_index)
            .context("retention capture cursor out of range")?
            .revision;
        let collection = *COLLECTIONS
            .get(before.collection_index)
            .context("retention collection cursor out of range")?;
        let cursor: Option<String> = self.db.query_row(
            "SELECT cursor FROM migration_retention WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        let after: Option<Cursor> = cursor.as_deref().map(serde_json::from_str).transpose()?;
        // Amortize durable commits over ordinary metadata rows. Stop at the first
        // external field so there is at most one pending record, and never advance
        // the source cursor past data whose bytes are not yet in custody.
        let page = source.page(revision, collection, after.as_ref(), 64)?;
        ensure!(
            page.records.len() <= 64,
            "source exceeded retention batch limit"
        );
        let mut staged = Vec::new();
        let mut staged_bytes = 0usize;
        for record in &page.records {
            let index = super::lookup::PreparedIndex::new(record)?;
            let raw = index.canonical_bytes();
            ensure!(raw.len() <= RECORD_LIMIT, "retained record size limit");
            if !staged.is_empty() && staged_bytes.saturating_add(raw.len()) > RECORD_LIMIT {
                break;
            }
            staged_bytes += raw.len();
            let compressed = compress(raw)?;
            let next = Cursor {
                seal: id.into(),
                revision: revision.clone(),
                collection,
                after: record.key.clone(),
            };
            let pending = record
                .fields
                .values()
                .any(|v| matches!(v, Field::Bytes(r) if r.bytes > 0));
            staged.push((
                record,
                compressed,
                raw.len(),
                blake3::hash(raw).to_hex().to_string(),
                serde_json::to_string(&next)?,
                pending,
                index,
            ));
            if pending {
                break;
            }
        }
        ensure!(
            !staged.is_empty() || page.exhausted,
            "empty source page without exhaustion"
        );
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            progress(&tx, id)? == before,
            "migration advanced concurrently; retry step"
        );
        let current: Option<String> = tx.query_row(
            "SELECT cursor FROM migration_retention WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            current == cursor,
            "migration cursor advanced concurrently; retry step"
        );
        if staged.is_empty() {
            let next_collection = before.collection_index + 1;
            let capture = before.capture_index + usize::from(next_collection == COLLECTIONS.len());
            tx.execute("UPDATE migration_retention SET capture_index=?2,collection_index=?3,cursor=NULL,complete=?4 WHERE id=?1",
                params![id,i64::try_from(capture)?,i64::try_from(next_collection%COLLECTIONS.len())?,capture==source.seal().selected.len()])?;
        } else {
            for (record, compressed, length, digest, next, pending, index) in staged {
                // Cursor and row commit atomically; a previously committed row
                // here is corruption, not an opportunity to silently skip bytes.
                tx.execute("INSERT INTO migration_retained_records(input,revision,collection,source_rowid,compressed,raw_length,digest,next_cursor,complete) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id,revision,i64::try_from(before.collection_index)?,record.rowid,compressed,i64::try_from(length)?,digest,next,!pending])?;
                let sequence = tx.last_insert_rowid();
                super::lookup::index_record(&tx, sequence, &index)?;
                for (field, value) in &record.fields {
                    if let Field::Bytes(reference) = value {
                        let descriptor = serde_json::to_vec(reference)?;
                        let evidence = evidence::begin_owned(
                            &tx,
                            &descriptor,
                            reference.bytes,
                            evidence::Authority::SelectedSource,
                        )?;
                        tx.execute(
                            "INSERT INTO migration_retained_fields VALUES(?1,?2,?3)",
                            params![sequence, field, evidence.id],
                        )?;
                    }
                }
                if !pending {
                    tx.execute(
                        "UPDATE migration_retention SET cursor=?2,records=records+1 WHERE id=?1",
                        params![id, next],
                    )?;
                }
            }
        }
        let result = progress(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    /// Completed raw records and their field evidence remain browsable after the
    /// inspection snapshot is disconnected. Decoding is bounded per record.
    pub fn retained_migration_records(
        &self,
        input: &str,
        revision: &str,
        collection: Collection,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, EvidenceRecord)>> {
        ensure!(
            after >= 0 && (1..=100).contains(&limit),
            "retained record page bounds"
        );
        let index = COLLECTIONS
            .iter()
            .position(|v| *v == collection)
            .context("unknown collection")?;
        let mut stmt=self.db.prepare("SELECT sequence,compressed,raw_length,digest FROM migration_retained_records WHERE input=?1 AND revision=?2 AND collection=?3 AND sequence>?4 AND complete=1 ORDER BY sequence LIMIT ?5")?;
        let mut rows = stmt.query(params![
            input,
            revision,
            i64::try_from(index)?,
            after,
            i64::try_from(limit)?
        ])?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let length = evidence::size(row, 2)?;
            if !result.is_empty() && bytes.saturating_add(length) > RECORD_LIMIT {
                break;
            }
            result.push((
                row.get(0)?,
                decode(
                    &row.get::<_, Vec<u8>>(1)?,
                    length,
                    &row.get::<_, String>(3)?,
                )?,
            ));
            bytes += length;
        }
        Ok(result)
    }

    pub fn retained_migration_field(
        &self,
        record: i64,
        field: &str,
    ) -> Result<evidence::EvidenceState> {
        let id:String=self.db.query_row("SELECT f.evidence FROM migration_retained_fields f JOIN migration_retained_records r ON r.sequence=f.record WHERE f.record=?1 AND f.field=?2 AND r.complete=1",params![record,field],|r|r.get(0))?;
        self.migration_evidence(&id)
    }
}
