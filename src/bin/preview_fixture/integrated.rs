//! Source-preserving, three-column overlay on an externally verified raw copy.
//! This is experiment setup, never an import or organization-fidelity claim.
use super::*;
use preview_fixture::{IdScheme, distinct_jpeg, verify_object};

const INDEX_SQL: &str =
    "CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)";
const TOTAL: u64 = 10_000_000;
const OFFLINE: &str = "/synthetic";
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Row {
    sequence: i64,
    id: String,
    location: Vec<u8>,
    path_display: String,
    fingerprint: Option<String>,
    state: String,
    metadata: Option<String>,
    preview_hash: Option<String>,
    error: Option<String>,
    render_generation: i64,
}
fn rows(db: &Connection, window: u32) -> Result<Vec<Row>> {
    let mut stmt = db.prepare("SELECT sequence,id,location,path_display,fingerprint,state,metadata,preview_hash,error,render_generation FROM assets WHERE sequence<=?1 ORDER BY sequence")?;
    Ok(stmt
        .query_map([window], |r| {
            Ok(Row {
                sequence: r.get(0)?,
                id: r.get(1)?,
                location: r.get(2)?,
                path_display: r.get(3)?,
                fingerprint: r.get(4)?,
                state: r.get(5)?,
                metadata: r.get(6)?,
                preview_hash: r.get(7)?,
                error: r.get(8)?,
                render_generation: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
fn hash<T: Serialize>(value: &T) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(value)?)
        .to_hex()
        .to_string())
}
type SchemaEntry = (String, String, String, Option<String>);
fn schema(db: &Connection) -> Result<Vec<SchemaEntry>> {
    let mut stmt =
        db.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
fn count_schema(db: &Connection, total: u64) -> Result<()> {
    ensure!(
        db.query_row::<u32, _, _>("PRAGMA user_version", [], |r| r.get(0))? == 5,
        "schema must be exactly 5; no migration in this experiment"
    );
    ensure!(
        db.query_row::<String, _, _>(
            "SELECT sql FROM sqlite_schema WHERE name='organization_lens_capture'",
            [],
            |r| r.get(0)
        )? == INDEX_SQL,
        "schema5 lens/capture index differs"
    );
    ensure!(
        db.query_row::<u32, _, _>("PRAGMA application_id", [], |r| r.get(0))? == 1346913089,
        "wrong application identity"
    );
    ensure!(
        u64::try_from(db.query_row::<i64, _, _>("SELECT count(*) FROM assets", [], |r| r.get(0))?)?
            == total,
        "wrong catalog count"
    );
    Ok(())
}
fn paths(rows: &[Row], window: u32, offline: &Path) -> Result<()> {
    ensure!(
        !offline.exists(),
        "actual synthetic source root exists; offline proof refused"
    );
    ensure!(rows.len() == window as usize, "missing overlay rows");
    for (index, row) in rows.iter().enumerate() {
        let sequence = index as i64 + 1;
        let expected = offline.join(format!("folder{}/file{sequence:012}.jpg", sequence % 5));
        let expected = expected.to_str().context("synthetic path UTF8")?;
        ensure!(
            row.sequence == sequence
                && row.id == format!("fixture-{sequence:012}")
                && row.state == "ready"
                && row.path_display == expected
                && row.location == expected.as_bytes(),
            "preserved source identity/path formula mismatch"
        );
    }
    Ok(())
}
/// Recheck actual preserved rows before any timed retained reads; no original file is opened.
pub(super) fn verify_offline(db: &Connection, data: &Dataset) -> Result<()> {
    let actual = rows(db, ASSETS)?;
    paths(&actual, ASSETS, Path::new(OFFLINE))?;
    for (index, row) in actual.iter().enumerate() {
        let k = key(data, index as u32);
        ensure!(
            row.fingerprint.as_deref() == Some(k.fingerprint.as_str())
                && row.render_generation == i64::try_from(k.generation)?
                && row.preview_hash.as_deref() == Some(k.digest()?.as_str()),
            "overlay render key mismatch"
        );
    }
    Ok(())
}
fn overlay(
    db: &mut Connection,
    keys: &[PreviewKey],
    total: u64,
    offline: &Path,
    interruption: impl FnOnce(&Connection) -> Result<()>,
) -> Result<Value> {
    let window = u32::try_from(keys.len())?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    count_schema(&tx, total)?;
    let before = rows(&tx, window)?;
    paths(&before, window, offline)?;
    let schema_before = schema(&tx)?;
    let sequence_before: Option<i64> = tx.query_row(
        "SELECT seq FROM sqlite_sequence WHERE name='assets'",
        [],
        |r| r.get(0),
    )?;
    let epoch: i64 = tx.query_row("SELECT revision FROM storage_epoch WHERE id=1", [], |r| {
        r.get(0)
    })?;
    let changes: i64 = tx.query_row("SELECT total_changes()", [], |r| r.get(0))?;
    let mut expected = before.clone();
    for (row, k) in expected.iter_mut().zip(keys) {
        ensure!(
            k.asset_id == row.id
                && row.fingerprint.as_deref() == Some("synthetic-fingerprint")
                && row.preview_hash.as_deref() == Some("synthetic-preview-no-object")
                && row.render_generation == 0,
            "unexpected prior identity or overlay key"
        );
        row.fingerprint = Some(k.fingerprint.clone());
        row.render_generation = i64::try_from(k.generation)?;
        row.preview_hash = Some(k.digest()?);
        ensure!(tx.execute("UPDATE assets SET fingerprint=?1,render_generation=?2,preview_hash=?3 WHERE sequence=?4 AND id=?5",params![row.fingerprint,row.render_generation,row.preview_hash,row.sequence,row.id])?==1,"overlay row missing");
    }
    interruption(&tx)?;
    let after = rows(&tx, window)?;
    ensure!(after == expected, "overlay changed another column");
    let epoch_after: i64 =
        tx.query_row("SELECT revision FROM storage_epoch WHERE id=1", [], |r| {
            r.get(0)
        })?;
    let changes_after: i64 = tx.query_row("SELECT total_changes()", [], |r| r.get(0))?;
    ensure!(
        epoch_after - epoch == i64::from(window)
            && changes_after - changes == 2 * i64::from(window),
        "unexpected DML/trigger effects"
    );
    ensure!(
        schema(&tx)? == schema_before
            && tx.query_row::<Option<i64>, _, _>(
                "SELECT seq FROM sqlite_sequence WHERE name='assets'",
                [],
                |r| r.get(0)
            )? == sequence_before,
        "schema/sequence changed"
    );
    count_schema(&tx, total)?;
    let receipt = json!({"changed_columns":["fingerprint","render_generation","preview_hash"],"rows":window,"catalog_count":total,"schema_version":5,"rows_before_blake3":hash(&before)?,"rows_after_blake3":hash(&after)?,"keys_blake3":hash(&keys)?,"schema_blake3":hash(&schema_before)?,"storage_epoch_before":epoch,"storage_epoch_after":epoch_after,"connection_total_changes_delta":changes_after-changes,"remaining_asset_columns_unchanged":true,"sqlite_sequence_unchanged":true,"actual_offline_root":offline,"source_path_formula":"/synthetic/folder{sequence%5}/file{sequence:012}.jpg","all_overlay_source_paths_verified":true,"organization_effect":"none: only storage_asset_change fires; exact 2*window DML, existing schema unchanged"});
    tx.commit()?;
    Ok(receipt)
}
pub(super) fn run(bundle: &Path, dataset_path: &Path) -> Result<()> {
    ensure!(
        bundle.is_absolute() && dataset_path.is_absolute(),
        "absolute paths required"
    );
    let copied = bundle.join("catalog/catalog.sqlite3");
    let proof: Value =
        serde_json::from_slice(&read_bounded(&bundle.join("copy-receipt.json"), 65536)?)?;
    ensure!(
        proof["complete"] == true
            && proof["schema_version"] == 5
            && proof["ancestry"]["complete"] == true
            && proof["copied_catalog"] == copied.to_string_lossy().as_ref(),
        "verified raw-copy receipt required"
    );
    let original = PathBuf::from(
        proof["source_catalog"]
            .as_str()
            .context("source receipt path")?,
    );
    ensure!(
        fs::canonicalize(&copied)? != fs::canonicalize(original)?
            && !fs::symlink_metadata(&copied)?.file_type().is_symlink(),
        "owned regular copy required"
    );
    let started = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"started":anchor(started),"source_copy_receipt_blake3":blake3::hash(&read_bounded(&bundle.join("copy-receipt.json"),65536)?).to_hex().to_string(),"kind":"synthetic preview identity overlay; no import/organization fidelity claim"});
    let result = (|| -> Result<()> {
        let mut data = dataset(dataset_path)?;
        ensure!(
            data.count == ASSETS && data.id_scheme == IdScheme::Layout,
            "fixed 10k original layout dataset required"
        );
        let mut db =
            Connection::open_with_flags(&copied, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        configure_catalog_connection(&db)?;
        count_schema(&db, TOTAL)?;
        paths(&rows(&db, ASSETS)?, ASSETS, Path::new(OFFLINE))?;
        let cache = bundle.join("preview-cache");
        fs::create_dir(&cache)?;
        data.id_scheme = IdScheme::OrganizationFixture;
        data.store.manifest_root = cache.join("manifest");
        data.store.thumbnail_root = cache.join("thumbnails");
        data.store.large_root = cache.join("large");
        let store = PreviewStore::open(data.store.clone(), &[PathBuf::from(OFFLINE)])?;
        let mut payloads = Vec::new();
        for seed in &data.seeds {
            let bytes = read_bounded(&seed.encoded_path, 8 * 1024 * 1024)?;
            ensure!(
                blake3::hash(&bytes).to_hex().as_str() == seed.encoded_blake3,
                "seed bytes changed"
            );
            let decoded = decode(&bytes, Codec::Jpeg)?;
            ensure!(
                decoded.width() == seed.width
                    && decoded.height() == seed.height
                    && decoded.digest() == seed.decoded_blake3,
                "seed RGB identity changed"
            );
            payloads.push(bytes);
        }
        let mut unique = HashSet::new();
        let mut keys = Vec::new();
        for index in 0..ASSETS {
            let k = key(&data, index);
            let seed = &data.seeds[index as usize % 30];
            let bytes = distinct_jpeg(&payloads[index as usize % 30], index)?;
            ensure!(
                unique.insert(verify_object(&bytes, index, &seed.encoded_blake3)?),
                "duplicate generated payload"
            );
            store.desire(&k, || Ok(true))?;
            ensure!(
                store.publish_record(&k, &bytes, &seed.record, |attach| attach())?
                    == Publication::Attached,
                "overlay preview publication failed"
            );
            keys.push(k);
            receipt["published_objects"] = json!(index + 1);
        }
        ensure!(
            store.usage()?.objects == u64::from(ASSETS),
            "wrong retained object count"
        );
        receipt["overlay"] = overlay(&mut db, &keys, TOTAL, Path::new(OFFLINE), |_| Ok(()))?;
        receipt["distinct_encoded_objects"] = json!(unique.len());
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        let dataset_path = bundle.join("dataset.json");
        exclusive(&dataset_path, &serde_json::to_value(&data)?)?;
        let fixture = Fixture {
            version: 1,
            catalog_count: TOTAL,
            dataset: dataset_path.clone(),
            dataset_blake3: blake3::hash(&read_bounded(&dataset_path, 1024 * 1024)?)
                .to_hex()
                .to_string(),
            catalog: bundle.join("catalog"),
            offline_originals: PathBuf::from(OFFLINE),
            count: ASSETS,
        };
        exclusive(
            &bundle.join("fixture.json"),
            &serde_json::to_value(fixture)?,
        )?;
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = anchor(started);
    exclusive(&bundle.join("overlay-receipt.json"), &receipt)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Connection, Vec<PreviewKey>, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("catalog");
        drop(Catalog::open(&catalog).unwrap());
        let db = Connection::open(catalog.join("catalog.sqlite3")).unwrap();
        let offline = dir.path().join("synthetic-absent");
        let mut keys = Vec::new();
        for sequence in 1..=4 {
            let id = format!("fixture-{sequence:012}");
            let path = offline
                .join(format!("folder{}/file{sequence:012}.jpg", sequence % 5))
                .to_str()
                .unwrap()
                .to_owned();
            db.execute("INSERT INTO assets(sequence,id,location,path_display,fingerprint,state,metadata,preview_hash) VALUES(?1,?2,?3,?4,'synthetic-fingerprint','ready','{}','synthetic-preview-no-object')",params![sequence,id,path.as_bytes(),path]).unwrap();
            if sequence <= 2 {
                keys.push(PreviewKey {
                    asset_id: id,
                    variant_id: "original".into(),
                    generation: 1,
                    fingerprint: "a".repeat(64),
                    edit_revision: 0,
                    renderer_version: renderer_identity(),
                    preparation_version: PREPARATION_VERSION.into(),
                    tier: Tier::Thumbnail,
                    edge: 512,
                    encoding: CodecSettings {
                        codec: Codec::Jpeg,
                        quality: 80,
                    },
                });
            }
        }
        (dir, db, keys, offline)
    }
    #[test]
    fn only_three_fields_and_expected_trigger_change() {
        let (_dir, mut db, keys, offline) = fixture();
        let tail = rows(&db, 4).unwrap()[2..].to_vec();
        let dirty: i64 = db
            .query_row("SELECT count(*) FROM organization_dirty", [], |r| r.get(0))
            .unwrap();
        let receipt = overlay(&mut db, &keys, 4, &offline, |_| Ok(())).unwrap();
        assert_eq!(receipt["connection_total_changes_delta"], 4);
        assert_eq!(rows(&db, 4).unwrap()[2..], tail);
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT count(*) FROM organization_dirty", [], |r| r.get(0))
                .unwrap(),
            dirty
        );
    }
    #[test]
    fn wrong_count_schema_and_key_are_rejected_without_changes() {
        for mode in 0..3 {
            let (_dir, mut db, mut keys, offline) = fixture();
            if mode == 1 {
                db.pragma_update(None, "user_version", 6).unwrap();
            }
            if mode == 2 {
                keys[1].asset_id = "wrong".into();
            }
            let before = rows(&db, 4).unwrap();
            assert!(
                overlay(
                    &mut db,
                    &keys,
                    if mode == 0 { 5 } else { 4 },
                    &offline,
                    |_| Ok(())
                )
                .is_err()
            );
            assert_eq!(rows(&db, 4).unwrap(), before);
        }
    }
    #[test]
    fn extra_row_mutation_and_remaining_column_mutation_roll_back() {
        for sql in [
            "UPDATE assets SET error='unexpected' WHERE sequence=4",
            "UPDATE assets SET metadata='changed' WHERE sequence=1",
        ] {
            let (_dir, mut db, keys, offline) = fixture();
            let before = rows(&db, 4).unwrap();
            assert!(
                overlay(&mut db, &keys, 4, &offline, |db| {
                    db.execute(sql, [])?;
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(rows(&db, 4).unwrap(), before);
        }
    }
    #[test]
    fn actual_preserved_paths_and_absence_are_required() {
        let (_dir, mut db, keys, offline) = fixture();
        db.execute(
            "UPDATE assets SET path_display='/somewhere-else' WHERE sequence=1",
            [],
        )
        .unwrap();
        assert!(overlay(&mut db, &keys, 4, &offline, |_| Ok(())).is_err());
        let (_dir, mut db, keys, offline) = fixture();
        fs::create_dir(&offline).unwrap();
        assert!(overlay(&mut db, &keys, 4, &offline, |_| Ok(())).is_err());
    }
}
