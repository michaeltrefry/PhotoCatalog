//! Compressed, ordered evidence chunks stored in the destination transaction.
//!
//! A payload is visible as complete only after all declared bytes are committed.
//! The manifest digest binds the descriptor, byte count and ordered chunk hashes;
//! it is deliberately distinct from a digest of the original uncompressed file.

use crate::{Catalog, catalog_writer::Priority};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const CHUNK_BYTES: usize = 1024 * 1024;
pub(super) const DESCRIPTOR_BYTES: usize = 64 * 1024;

/// A private admission domain: public byte uploads never confer sealed-source custody.
#[derive(Clone, Copy)]
pub(crate) enum Authority {
    Generic,
    SelectedSource,
    CapturedArtifact,
}
impl Authority {
    fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::SelectedSource => "selected_source",
            Self::CapturedArtifact => "captured_artifact",
        }
    }
}
pub(crate) fn require_authority(db: &Connection, id: &str, authority: Authority) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT authority=?2 FROM migration_evidence WHERE id=?1",
            params![id, authority.name()],
            |r| r.get::<_, bool>(0)
        )?,
        "evidence admission authority differs"
    );
    Ok(())
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_evidence(
            id TEXT PRIMARY KEY, descriptor BLOB NOT NULL, authority TEXT NOT NULL CHECK(authority IN('generic','selected_source','captured_artifact')),
            length INTEGER NOT NULL CHECK(length>=0),
            committed INTEGER NOT NULL DEFAULT 0 CHECK(committed>=0 AND committed<=length),
            manifest TEXT NOT NULL, complete INTEGER NOT NULL CHECK(complete IN (0,1)));
         CREATE TABLE IF NOT EXISTS migration_evidence_blobs(
            hash TEXT PRIMARY KEY, length INTEGER NOT NULL CHECK(length>0 AND length<=1048576),
            compressed BLOB NOT NULL CHECK(length(compressed)<=1052672));
         CREATE TABLE IF NOT EXISTS migration_evidence_chunks(
            evidence TEXT NOT NULL REFERENCES migration_evidence(id),
            offset INTEGER NOT NULL CHECK(offset>=0),
            hash TEXT NOT NULL REFERENCES migration_evidence_blobs(hash),
            PRIMARY KEY(evidence,offset));",
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceState {
    pub id: String,
    pub length: u64,
    pub committed: u64,
    pub manifest: String,
    pub complete: bool,
}

/// Admit retained identities from borrowed SQLite bytes before allocating them.
pub(crate) fn retained_identity(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<String> {
    let value = row.get_ref(index)?;
    if let rusqlite::types::ValueRef::Text(bytes) = value
        && bytes.len() == 64
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        return Ok(text.to_owned());
    }
    Err(rusqlite::Error::FromSqlConversionFailure(
        index,
        value.data_type(),
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "retained identity must be 64 bytes of UTF-8 TEXT",
        )),
    ))
}

/// Validate stored descriptors while SQLite still owns the bytes.
pub(super) fn retained_descriptor<'r>(
    row: &'r rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<&'r [u8]> {
    let value = row.get_ref(index)?;
    if let rusqlite::types::ValueRef::Blob(bytes) = value
        && bytes.len() <= DESCRIPTOR_BYTES
    {
        return Ok(bytes);
    }
    Err(rusqlite::Error::FromSqlConversionFailure(
        index,
        value.data_type(),
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "retained descriptor must be a BLOB of at most 64 KiB",
        )),
    ))
}

pub(crate) fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

pub(crate) fn size(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    usize::try_from(row.get::<_, i64>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn initial_manifest(descriptor: &[u8], length: u64, authority: Authority) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"photocatalog-retained-evidence-v2\0");
    hash.update(authority.name().as_bytes());
    hash.update(b"\0");
    hash.update(&length.to_le_bytes());
    hash.update(descriptor);
    hash.finalize().to_hex().to_string()
}

fn next_manifest(previous: &str, offset: u64, length: usize, hash: &str) -> String {
    let mut result = blake3::Hasher::new();
    result.update(b"photocatalog-evidence-chunk-v1\0");
    result.update(previous.as_bytes());
    result.update(&offset.to_le_bytes());
    result.update(&(length as u64).to_le_bytes());
    result.update(hash.as_bytes());
    result.finalize().to_hex().to_string()
}

fn state(db: &Connection, id: &str) -> Result<EvidenceState> {
    ensure!(id.len() == 64, "evidence identity size limit");
    let (length, committed, manifest, complete): (u64, u64, Option<String>, bool) = db.query_row(
        "SELECT length,committed,
         CASE WHEN typeof(manifest)='text' AND length(CAST(manifest AS BLOB))=64 THEN manifest END,
         complete FROM migration_evidence WHERE id=?1",
        [id],
        |row| {
            Ok((
                unsigned(row, 0)?,
                unsigned(row, 1)?,
                row.get(2)?,
                row.get(3)?,
            ))
        },
    )?;
    Ok(EvidenceState {
        id: id.into(),
        length,
        committed,
        manifest: manifest.context("evidence manifest type/size limit")?,
        complete,
    })
}

/// Compression and hashing happen before admission to the catalog writer.
pub(crate) struct PreparedChunk {
    hash: String,
    length: usize,
    compressed: Vec<u8>,
}
impl PreparedChunk {
    #[cfg(all(test, feature = "internal-capacity-probes"))]
    pub(crate) fn probe_capacity(&self) -> usize {
        self.hash.capacity() + self.compressed.capacity()
    }
    pub(crate) fn new(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK_BYTES,
            "evidence chunk size"
        );
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes)?;
        Ok(Self {
            hash: blake3::hash(bytes).to_hex().to_string(),
            length: bytes.len(),
            compressed: encoder.finish()?,
        })
    }
}

pub(crate) fn begin(db: &Connection, descriptor: &[u8], length: u64) -> Result<EvidenceState> {
    begin_owned(db, descriptor, length, Authority::Generic)
}
pub(crate) fn begin_owned(
    db: &Connection,
    descriptor: &[u8],
    length: u64,
    authority: Authority,
) -> Result<EvidenceState> {
    ensure!(
        descriptor.len() <= DESCRIPTOR_BYTES,
        "evidence descriptor exceeds 64 KiB"
    );
    ensure!(
        length <= i64::MAX as u64,
        "evidence length exceeds SQLite range"
    );
    let id = initial_manifest(descriptor, length, authority);
    db.execute(
        "INSERT OR IGNORE INTO migration_evidence(id,descriptor,length,manifest,complete,authority) VALUES(?1,?2,?3,?1,?4,?5)",
        params![id, descriptor, i64::try_from(length)?, length == 0, authority.name()],
    )?;
    let matches: bool = db.query_row(
        "SELECT descriptor,length FROM migration_evidence WHERE id=?1",
        [&id],
        |r| Ok(retained_descriptor(r, 0)? == descriptor && unsigned(r, 1)? == length),
    )?;
    ensure!(matches, "evidence identity collision");
    require_authority(db, &id, authority)?;
    state(db, &id)
}

/// Call inside the same transaction as the importer checkpoint. Replaying an
/// already committed chunk is accepted only when its bytes have the same hash.
pub(crate) fn append(
    db: &Connection,
    id: &str,
    offset: u64,
    chunk: &PreparedChunk,
) -> Result<EvidenceState> {
    append_owned(db, id, offset, chunk, Authority::Generic)
}
pub(crate) fn append_owned(
    db: &Connection,
    id: &str,
    offset: u64,
    chunk: &PreparedChunk,
    authority: Authority,
) -> Result<EvidenceState> {
    require_authority(db, id, authority)?;
    let before = state(db, id)?;
    ensure!(offset <= before.length, "evidence offset exceeds length");
    ensure!(
        chunk.length as u64 <= before.length - offset,
        "evidence chunk exceeds declared payload length"
    );
    if offset < before.committed {
        let (hash, length): (String, usize) = db.query_row(
            "SELECT c.hash,b.length FROM migration_evidence_chunks c JOIN migration_evidence_blobs b ON b.hash=c.hash WHERE c.evidence=?1 AND c.offset=?2",
            params![id, i64::try_from(offset)?],
            |r| Ok((r.get(0)?, size(r,1)?)),
        )?;
        ensure!(
            hash == chunk.hash && length == chunk.length,
            "replayed evidence bytes differ"
        );
        return Ok(before);
    }
    ensure!(
        offset == before.committed && !before.complete,
        "evidence chunk is out of order"
    );
    db.execute(
        "INSERT OR IGNORE INTO migration_evidence_blobs VALUES(?1,?2,?3)",
        params![chunk.hash, i64::try_from(chunk.length)?, chunk.compressed],
    )?;
    db.execute(
        "INSERT INTO migration_evidence_chunks VALUES(?1,?2,?3)",
        params![id, i64::try_from(offset)?, chunk.hash],
    )?;
    let committed = offset + chunk.length as u64;
    let manifest = next_manifest(&before.manifest, offset, chunk.length, &chunk.hash);
    db.execute(
        "UPDATE migration_evidence SET committed=?2,manifest=?3,complete=?4 WHERE id=?1",
        params![
            id,
            i64::try_from(committed)?,
            manifest,
            committed == before.length
        ],
    )?;
    state(db, id)
}

pub(crate) fn read(db: &Connection, id: &str, offset: u64) -> Result<Vec<u8>> {
    let status = state(db, id)?;
    ensure!(status.complete, "evidence is not complete");
    ensure!(offset <= status.length, "evidence chunk offset");
    if offset == status.length {
        return Ok(Vec::new());
    }
    let (hash, length, compressed): (Option<String>, u64, Option<Vec<u8>>) = db.query_row(
        "SELECT CASE WHEN typeof(b.hash)='text' AND length(CAST(b.hash AS BLOB))=64 THEN b.hash END,
         b.length, CASE WHEN typeof(b.compressed)='blob' AND length(b.compressed)<=?3
          AND typeof(b.length)='integer' AND b.length BETWEEN 1 AND ?4 THEN b.compressed END
         FROM migration_evidence_chunks c
         JOIN migration_evidence_blobs b ON b.hash=c.hash WHERE c.evidence=?1 AND c.offset=?2",
        params![id, i64::try_from(offset)?, i64::try_from(CHUNK_BYTES + 4096)?, i64::try_from(CHUNK_BYTES)?],
        |r| Ok((r.get(0)?, unsigned(r, 1)?, r.get(2)?)),
    )?;
    let hash = hash.context("evidence chunk digest type/size limit")?;
    let compressed = compressed.context("compressed evidence type/size limit")?;
    ensure!(
        length > 0 && length <= (status.length - offset).min(CHUNK_BYTES as u64),
        "evidence chunk length differs"
    );
    ensure!(
        compressed.len() <= CHUNK_BYTES + 4096,
        "compressed evidence size limit"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    let mut decoder = ZlibDecoder::new(compressed.as_slice());
    (&mut decoder).take(length + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == length
            && decoder.total_in() == compressed.len() as u64
            && blake3::hash(&bytes).to_hex().as_str() == hash,
        "retained evidence integrity mismatch"
    );
    Ok(bytes)
}

impl Catalog {
    pub fn begin_migration_evidence(
        &mut self,
        descriptor: &[u8],
        length: u64,
    ) -> Result<EvidenceState> {
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = begin(&tx, descriptor, length)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn append_migration_evidence(
        &mut self,
        id: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<EvidenceState> {
        let chunk = PreparedChunk::new(bytes)?;
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = append(&tx, id, offset, &chunk)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn migration_evidence(&self, id: &str) -> Result<EvidenceState> {
        state(&self.db, id)
    }

    pub fn migration_evidence_descriptor(&self, id: &str) -> Result<Vec<u8>> {
        self.db
            .query_row(
                "SELECT descriptor FROM migration_evidence WHERE id=?1",
                [id],
                |r| Ok(retained_descriptor(r, 0)?.to_vec()),
            )
            .optional()?
            .context("unknown migration evidence")
    }

    /// Read one verified chunk; no API materializes the entire payload.
    pub fn migration_evidence_chunk(&self, id: &str, offset: u64) -> Result<Vec<u8>> {
        read(&self.db, id, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_descriptors_admit_blob_size_before_copy_or_comparison() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let descriptor = vec![0xff; DESCRIPTOR_BYTES];
        let before = catalog.begin_migration_evidence(&descriptor, 1)?;
        assert_eq!(
            catalog.migration_evidence_descriptor(&before.id)?,
            descriptor
        );
        assert_eq!(catalog.begin_migration_evidence(&descriptor, 1)?, before);
        for expression in ["zeroblob(65537)", "zeroblob(1048576)", "'text'", "64"] {
            catalog.db.execute_batch("SAVEPOINT malformed_descriptor")?;
            catalog.db.execute(
                &format!("UPDATE migration_evidence SET descriptor={expression} WHERE id=?1"),
                [&before.id],
            )?;
            for error in [
                begin(&catalog.db, &descriptor, 1).unwrap_err(),
                catalog
                    .migration_evidence_descriptor(&before.id)
                    .unwrap_err(),
            ] {
                assert!(format!("{error:#}").contains("BLOB of at most 64 KiB"));
            }
            catalog
                .db
                .execute_batch("ROLLBACK TO malformed_descriptor; RELEASE malformed_descriptor")?;
            assert_eq!(
                catalog.migration_evidence_descriptor(&before.id)?,
                descriptor
            );
        }
        let empty = catalog.begin_migration_evidence(b"", 0)?;
        assert!(catalog.migration_evidence_descriptor(&empty.id)?.is_empty());
        assert!(
            catalog
                .migration_evidence_descriptor(&"x".repeat(64))
                .unwrap_err()
                .to_string()
                .contains("unknown migration evidence")
        );
        Ok(())
    }

    #[test]
    fn retained_identity_checks_bytes_storage_and_utf8_without_normalizing() -> Result<()> {
        let db = Connection::open_in_memory()?;
        for value in ["g".repeat(64), "é".repeat(32)] {
            let actual = db.query_row("SELECT ?", [&value], |r| retained_identity(r, 0))?;
            assert_eq!(actual, value);
        }
        for expression in [
            "NULL",
            "64",
            "zeroblob(64)",
            "hex(zeroblob(32)) || 'x'",
            "replace(hex(zeroblob(33)), '0', 'é')",
            "CAST(x'ff' || zeroblob(63) AS TEXT)",
            "'short'",
        ] {
            let error = db
                .query_row(&format!("SELECT {expression}"), [], |r| {
                    retained_identity(r, 0)
                })
                .unwrap_err();
            assert!(error.to_string().contains("64 bytes of UTF-8 TEXT"));
        }
        Ok(())
    }

    #[test]
    fn opening_evidence_columns_admit_bytes_and_storage_before_materialization() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.pragma_update(None, "foreign_keys", true)?;
        install(&db)?;
        let id = begin(&db, b"bounded opening", 3)?.id;
        append(&db, &id, 0, &PreparedChunk::new(b"yes")?)?;
        for (sql, expected) in [
            (
                "UPDATE migration_evidence SET manifest=replace(hex(zeroblob(33)),'0','é')",
                "manifest type/size",
            ),
            (
                "UPDATE migration_evidence SET manifest=zeroblob(64)",
                "manifest type/size",
            ),
            (
                "UPDATE migration_evidence_blobs SET hash=replace(hex(zeroblob(33)),'0','é'); UPDATE migration_evidence_chunks SET hash=(SELECT hash FROM migration_evidence_blobs)",
                "digest type/size",
            ),
            (
                "UPDATE migration_evidence_blobs SET compressed=replace(hex(zeroblob(300000)),'0','é')",
                "compressed evidence type/size",
            ),
            (
                "UPDATE migration_evidence_blobs SET compressed='bad'",
                "compressed evidence type/size",
            ),
        ] {
            db.execute_batch("SAVEPOINT corrupt; PRAGMA defer_foreign_keys=ON")?;
            db.execute_batch(sql)?;
            let error = read(&db, &id, 0).unwrap_err();
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
            assert_eq!(read(&db, &id, 0)?, b"yes");
        }
        assert!(
            state(&db, &"é".repeat(64))
                .unwrap_err()
                .to_string()
                .contains("identity size")
        );
        Ok(())
    }

    #[test]
    fn admission_domains_cannot_cross_public_append_authority() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let descriptor = b"same public descriptor";
        let generic = catalog.begin_migration_evidence(descriptor, 3)?;
        catalog.append_migration_evidence(&generic.id, 0, b"bad")?;
        for authority in [Authority::SelectedSource, Authority::CapturedArtifact] {
            let owned = begin_owned(&catalog.db, descriptor, 3, authority)?;
            assert_ne!(generic.id, owned.id);
            assert!(!owned.complete);
            assert!(
                catalog
                    .append_migration_evidence(&owned.id, 0, b"bad")
                    .is_err()
            );
            assert_eq!(catalog.migration_evidence(&owned.id)?, owned);
            append_owned(
                &catalog.db,
                &owned.id,
                0,
                &PreparedChunk::new(b"yes")?,
                authority,
            )?;
            assert_eq!(catalog.migration_evidence_chunk(&owned.id, 0)?, b"yes");
            assert!(
                catalog
                    .append_migration_evidence(&owned.id, 0, b"yes")
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn interrupted_large_payload_resumes_without_duplicate_chunks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("evidence.sqlite");
        let mut db = Connection::open(&file)?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        install(&db)?;
        let size = 17 * CHUNK_BYTES + 7;
        let id = begin(&db, b"sealed-source/oversized-history", size as u64)?.id;
        let full = PreparedChunk::new(&vec![0xa7; CHUNK_BYTES])?;
        for index in 0..3 {
            let tx = db.transaction()?;
            append(&tx, &id, (index * CHUNK_BYTES) as u64, &full)?;
            tx.commit()?;
        }
        {
            let tx = db.transaction()?;
            append(&tx, &id, (3 * CHUNK_BYTES) as u64, &full)?;
            // Interruption rolls back both bytes and cursor.
        }
        drop(db);
        let mut db = Connection::open(&file)?;
        assert_eq!(
            begin(&db, b"sealed-source/oversized-history", size as u64)?.committed,
            (3 * CHUNK_BYTES) as u64
        );
        assert!(read(&db, &id, 0).is_err());
        for index in 0..17 {
            let tx = db.transaction()?;
            append(&tx, &id, (index * CHUNK_BYTES) as u64, &full)?;
            tx.commit()?;
        }
        let final_chunk = PreparedChunk::new(b"the end")?;
        let tx = db.transaction()?;
        let finished = append(&tx, &id, (17 * CHUNK_BYTES) as u64, &final_chunk)?;
        tx.commit()?;
        assert!(finished.complete);
        assert_eq!(read(&db, &id, (17 * CHUNK_BYTES) as u64)?, b"the end");
        assert_eq!(read(&db, &id, 0)?, vec![0xa7; CHUNK_BYTES]);
        assert_eq!(
            db.query_row("SELECT count(*) FROM migration_evidence_blobs", [], |r| r
                .get::<_, i64>(
                0
            ))?,
            2
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM migration_evidence_chunks", [], |r| {
                r.get::<_, i64>(0)
            })?,
            18
        );
        Ok(())
    }

    #[test]
    fn rejects_changed_replay_skips_and_corrupt_retained_bytes() -> Result<()> {
        let db = Connection::open_in_memory()?;
        install(&db)?;
        let id = begin(&db, b"packet", (CHUNK_BYTES + 1) as u64)?.id;
        let first = PreparedChunk::new(&vec![1; CHUNK_BYTES])?;
        let tail = PreparedChunk::new(&[2])?;
        assert!(append(&db, &id, CHUNK_BYTES as u64, &tail).is_err());
        append(&db, &id, 0, &first)?;
        assert!(append(&db, &id, 0, &PreparedChunk::new(&vec![3; CHUNK_BYTES])?).is_err());
        append(&db, &id, CHUNK_BYTES as u64, &tail)?;
        db.execute(
            "UPDATE migration_evidence_blobs SET compressed=x'00' WHERE hash=?1",
            [&tail.hash],
        )?;
        assert!(read(&db, &id, CHUNK_BYTES as u64).is_err());
        let empty = begin(&db, b"empty", 0)?;
        assert!(empty.complete);
        assert_eq!(read(&db, &empty.id, 0)?, Vec::<u8>::new());
        Ok(())
    }
}
