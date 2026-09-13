//! Explicit source-file mappings. Registration does no filesystem I/O.
use crate::{Catalog, catalog_writer::Priority, lightroom::plan::Cell, storage_volume::NativePath};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

/// Uses immutable capture bytes and the original typed row key, independent of
/// the random lineage assigned when a capture enters an inspection plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceKey {
    pub capture_revision: String,
    pub table: String,
    pub key: Vec<Cell>,
}
impl SourceKey {
    pub fn identity(&self) -> Result<String> {
        ensure!(
            self.capture_revision.len() == 64
                && self
                    .capture_revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid capture revision"
        );
        ensure!(
            !self.table.is_empty() && self.table.len() <= 1024 && !self.table.contains('\0'),
            "source table bounds"
        );
        ensure!(
            !self.key.is_empty() && self.key.len() <= 128,
            "source key bounds"
        );
        // Check before serialization: blob/text keys can otherwise allocate an
        // arbitrarily large JSON string before the encoded-byte bound is applied.
        let bytes = self.key.iter().try_fold(0usize, |sum, cell| {
            let size = match cell {
                Cell::Text(v) | Cell::Blob(v) => v.len(),
                _ => 8,
            };
            sum.checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("source key overflow"))
        })?;
        ensure!(bytes <= 16384, "source key bytes exceed limit");
        let mut hash = blake3::Hasher::new();
        hash.update(b"photocatalog-captured-row-v1\0");
        hash.update(&serde_json::to_vec(self)?);
        Ok(hash.finalize().to_hex().to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum OriginalDecision {
    /// Refuses a location already in the catalog. The coordinator must supply a
    /// reviewed Reuse decision for an overlap; path equality never merges images.
    Create { path: NativePath },
    Reuse {
        asset_id: String,
        expected_path: NativePath,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalRequest {
    pub import_source: String,
    pub source: SourceKey,
    pub decision: OriginalDecision,
    /// A completed selected AgLibraryFile row in destination evidence custody.
    pub retained_record: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalMapping {
    pub source_identity: String,
    pub asset_id: String,
    pub created: bool,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS migration_originals(
        source_identity TEXT PRIMARY KEY, import_source TEXT NOT NULL,
        source_json TEXT NOT NULL, decision_json TEXT NOT NULL,
        retained_record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        asset_id TEXT NOT NULL REFERENCES assets(id), created INTEGER NOT NULL CHECK(created IN(0,1)));")?;
    Ok(())
}

pub(crate) fn register(db: &Connection, request: &OriginalRequest) -> Result<OriginalMapping> {
    ensure!(
        !request.import_source.is_empty()
            && request.import_source.len() <= 4096
            && !request.import_source.contains('\0'),
        "import owner bounds"
    );
    let identity = request.source.identity()?;
    let retained = super::retention::selected_record(db, request.retained_record)?;
    ensure!(
        retained.collection == crate::lightroom::migration_source::Collection::Rows
            && retained.revision == request.source.capture_revision,
        "source record revision/type differs"
    );
    let table =
        super::retention::field_bytes(db, request.retained_record, &retained, "table_name", 1024)?;
    ensure!(
        table == b"AgLibraryFile" && table == request.source.table.as_bytes(),
        "original registration requires the selected file row"
    );
    let key = super::retention::field_bytes(
        db,
        request.retained_record,
        &retained,
        "key_json",
        64 * 1024,
    )?;
    ensure!(
        serde_json::from_slice::<Vec<Cell>>(&key)? == request.source.key,
        "source record key differs"
    );
    let path = match &request.decision {
        OriginalDecision::Create { path } => path,
        OriginalDecision::Reuse {
            asset_id,
            expected_path,
        } => {
            ensure!(
                !asset_id.is_empty() && asset_id.len() <= 4096,
                "reused asset identity bounds"
            );
            expected_path
        }
    };
    let units = match path {
        NativePath::UnixBytes(v) => v.len(),
        NativePath::WindowsWide(v) => v.len(),
    };
    ensure!((1..=32768).contains(&units), "original path length bounds");
    let source_json = serde_json::to_string(&request.source)?;
    let decision_json = serde_json::to_string(&request.decision)?;
    let existing: Option<(String, String, String, String, bool)> = db.query_row(
        "SELECT source_json,decision_json,import_source,asset_id,created FROM migration_originals WHERE source_identity=?1",
        [&identity], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
    if let Some((source, decision, owner, asset_id, created)) = existing {
        ensure!(
            source == source_json && decision == decision_json && owner == request.import_source,
            "source original mapping differs; explicit reconciliation required"
        );
        // Relinking after migration is legitimate. An identical replay keeps the
        // stable mapping without restoring the old locator or probing originals.
        return Ok(OriginalMapping {
            source_identity: identity,
            asset_id,
            created,
        });
    }
    let (asset_id, created) = match &request.decision {
        OriginalDecision::Create { path } => {
            let asset_id = uuid::Uuid::new_v4().to_string();
            crate::catalog_images::reserve_import_asset(db, &asset_id, &request.import_source)?;
            let display = match path {
                NativePath::UnixBytes(v) => String::from_utf8_lossy(v).into_owned(),
                NativePath::WindowsWide(v) => String::from_utf16_lossy(v),
            };
            db.execute(
                "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?3,'pending')",
                params![
                    asset_id,
                    crate::catalog_storage::encoded_bytes(path),
                    display
                ],
            )?;
            crate::catalog_storage::record_storage_path(db, &asset_id, path)?;
            (asset_id, true)
        }
        OriginalDecision::Reuse {
            asset_id,
            expected_path,
        } => {
            // This helper verifies both raw locator bytes and native encoding.
            crate::catalog_storage::record_storage_path(db, asset_id, expected_path)?;
            (asset_id.clone(), false)
        }
    };
    db.execute(
        "INSERT INTO migration_originals VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            identity,
            request.import_source,
            source_json,
            decision_json,
            request.retained_record,
            asset_id,
            created
        ],
    )?;
    Ok(OriginalMapping {
        source_identity: identity,
        asset_id,
        created,
    })
}

impl Catalog {
    pub fn register_migration_original(
        &mut self,
        request: &OriginalRequest,
    ) -> Result<OriginalMapping> {
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mapping = register(&tx, request)?;
        tx.commit()?;
        Ok(mapping)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_source_key_keeps_sqlite_types_and_capture_revisions_distinct() -> Result<()> {
        let source = SourceKey {
            capture_revision: "a".repeat(64),
            table: "AgLibraryFile".into(),
            key: vec![Cell::Integer(7)],
        };
        let mut other = source.clone();
        other.key = vec![Cell::Text(b"7".to_vec())];
        assert_ne!(source.identity()?, other.identity()?);
        other = source.clone();
        other.capture_revision = "b".repeat(64);
        assert_ne!(source.identity()?, other.identity()?);
        assert_eq!(source.identity()?, source.clone().identity()?);
        Ok(())
    }

    #[test]
    fn offline_registration_replay_and_explicit_overlap_do_not_move_originals() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut catalog = Catalog::open(dir.path().join("catalog"))?;
        let mut fixture = crate::lightroom::migration_source::tests::Fixture::new();
        let revision = fixture.revision().to_owned();
        fixture.edit(|db| {
            db.execute("UPDATE tables SET name='AgLibraryFile'",[]).unwrap();
            db.execute("UPDATE rows SET table_name='AgLibraryFile'",[]).unwrap();
            db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) SELECT revision,'second-file','AgLibraryFile','[{\"type\":\"Integer\",\"value\":8}]',cells_json FROM rows WHERE revision=?1",[&revision]).unwrap();
            db.execute("UPDATE tables SET expected=2,retained=2 WHERE revision=?1",[&revision]).unwrap();
        });
        let approval = b"selected synthetic originals";
        fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
        let source = fixture.open();
        catalog.begin_migration_retention(&source, approval)?;
        for _ in 0..100 {
            if catalog.step_migration_retention(&source)?.complete {
                break;
            }
        }
        assert!(
            catalog
                .migration_retention_progress(source.binding_blake3())?
                .complete
        );
        let records = catalog.retained_migration_records(
            source.binding_blake3(),
            &revision,
            crate::lightroom::migration_source::Collection::Rows,
            0,
            100,
        )?;
        assert_eq!(records.len(), 2);
        let missing = dir
            .path()
            .join("offline")
            .join("2014")
            .join("January")
            .join("photo.dng");
        let path = NativePath::from_path(&missing);
        let request = OriginalRequest {
            import_source: "test-selection".into(),
            source: SourceKey {
                capture_revision: revision,
                table: "AgLibraryFile".into(),
                key: vec![Cell::Integer(7)],
            },
            decision: OriginalDecision::Create { path: path.clone() },
            retained_record: records[0].0,
        };
        let first = catalog.register_migration_original(&request)?;
        assert!(first.created);
        assert_eq!(first, catalog.register_migration_original(&request)?);
        assert!(!missing.exists());
        let mut second = request.clone();
        second.source.key = vec![Cell::Integer(8)];
        second.retained_record = records[1].0;
        assert!(catalog.register_migration_original(&second).is_err());
        second.decision = OriginalDecision::Reuse {
            asset_id: first.asset_id.clone(),
            expected_path: path,
        };
        let reused = catalog.register_migration_original(&second)?;
        assert_eq!(reused.asset_id, first.asset_id);
        assert!(!reused.created);
        let mut wrong = request.clone();
        wrong.import_source = "another owner".into();
        assert!(catalog.register_migration_original(&wrong).is_err());
        wrong = request.clone();
        wrong.source.capture_revision = fixture.seal.excluded_revisions[0].clone();
        assert!(catalog.register_migration_original(&wrong).is_err());
        wrong = request.clone();
        wrong.retained_record = -1;
        assert!(catalog.register_migration_original(&wrong).is_err());
        wrong = request.clone();
        wrong.source.key = vec![Cell::Integer(9)];
        assert!(catalog.register_migration_original(&wrong).is_err());
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM assets", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            catalog
                .db
                .query_row("SELECT count(*) FROM image_import_reservations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            1
        );
        Ok(())
    }
}
