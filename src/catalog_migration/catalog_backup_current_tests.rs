use super::super::backup_snapshot;
use super::*;
use crate::catalog_backup::{Limits, backup_catalog, restore_catalog, restore_status};
use rusqlite::OptionalExtension;

#[test]
fn backup_preserves_current_repair_partial_archive_and_both_recipe_versions() -> Result<()> {
    let fixture = ImportFixture::with_wrapper(false, true)?;
    let (mut c, s, run) = old_complete(&fixture)?;
    let r = request(&c, &run.id)?;
    let repair = c.begin_current_develop_repair(&s, &r)?;
    let copies = tempfile::tempdir()?;
    let limits = Limits {
        min_free_bytes: 0,
        ..Default::default()
    };
    let key = fixture.key(&c, 0, 20)?;
    let mut phases = std::collections::BTreeSet::new();
    let mut saw_partial = false;
    for step in 0..64 {
        let p = c.current_develop_repair_progress(&repair.id)?;
        let first = phases.insert(format!("{:?}", p.phase));
        let partial = p.phase == current_repair::Phase::Project && p.repaired > 0 && p.examined < 6;
        if first || (partial && !saw_partial) {
            let before = backup_snapshot::logical_snapshot(&fixture.destination)?;
            let bundle = copies.path().join(format!("bundle-{step}"));
            let root = copies.path().join(format!("restored-{step}"));
            backup_catalog(&fixture.destination, &bundle, &limits, |_| Ok(()))?;
            restore_catalog(&bundle, &root, &limits, |_| Ok(()))?;
            assert_eq!(backup_snapshot::logical_snapshot(&root)?, before);
            assert_eq!(backup_snapshot::logical_snapshot(&bundle)?, before);
            let restored = Catalog::open(&root)?;
            assert_eq!(
                serde_json::to_value(restored.current_develop_repair_progress(&repair.id)?)?,
                serde_json::to_value(&p)?
            );
            assert_eq!(
                serde_json::to_value(restored.edit_variant(&key)?)?,
                serde_json::to_value(c.edit_variant(&key)?)?
            );
            assert!(restore_status(&root)?.unwrap().jobs_held);
            if p.after_record > 0 {
                let before = c.current_develop_repair_predecessor(&repair.id, p.after_record)?;
                let after =
                    restored.current_develop_repair_predecessor(&repair.id, p.after_record)?;
                assert_eq!(after.outcome, before.outcome);
                assert_eq!(after.metadata, before.metadata);
                assert_eq!(after.new_result_digest, before.new_result_digest);
                assert_eq!(after.new_outcome_digest, before.new_outcome_digest);
                assert_eq!(after.disposition, before.disposition);
            }
            assert_eq!(
                backup_snapshot::logical_snapshot(&fixture.destination)?,
                before
            );
            saw_partial |= partial;
        }
        if p.complete {
            break;
        }
        c.step_current_develop_repair(&s, &repair.id)?;
    }
    assert_eq!(
        phases,
        ["ArchiveReports", "Project", "Reconciliation", "Complete"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    assert!(saw_partial);
    Ok(())
}

#[test]
fn backup_preserves_real_pending_selected_record_chunks_without_source_access() -> Result<()> {
    use crate::lightroom::migration_source::{Field, MigrationSource, ReadLimits, tests::Fixture};
    use std::io::Read;
    let mut f = Fixture::with_large_cell(2 * 1024 * 1024);
    let approval = b"selected synthetic backup retention";
    f.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
    let source = MigrationSource::open(
        f.seal.clone(),
        ReadLimits {
            chunk_bytes: 256 * 1024,
            ..Default::default()
        },
    )?;
    let page = source.page(f.revision(), Collection::Rows, None, 1)?;
    let Field::Bytes(raw) = &page.records[0].fields["cells_json"] else {
        anyhow::bail!("fixture must expose chunked selected source bytes");
    };
    let expected = source.read_chunk(raw, 0, source.max_chunk_bytes())?;
    let input = source.binding_blake3().to_owned();
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.begin_migration_retention(&source, approval)?;
    let mut prefix = None;
    for _ in 0..100 {
        c.step_migration_retention(&source)?;
        prefix=c.db.query_row("SELECT e.id,e.committed FROM migration_evidence e JOIN migration_retained_fields f ON f.evidence=e.id JOIN migration_retained_records r ON r.sequence=f.record WHERE r.input=?1 AND r.complete=0 AND e.committed>0 AND e.complete=0 LIMIT 1",[&input],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).optional()?;
        if prefix.is_some() {
            break;
        }
    }
    let (id, committed) = prefix.context("fixture must retain a genuine partial field")?;
    let committed = u64::try_from(committed)?;
    let progress = c.migration_retention_progress(&input)?;
    assert!(!progress.complete);
    let evidence = c.migration_evidence(&id)?;
    assert!(committed > 0 && committed < evidence.length);
    // The public evidence reader correctly withholds incomplete payloads. Verify
    // the committed physical chunk against the selected source bytes instead.
    assert!(c.migration_evidence_chunk(&id, 0).is_err());
    let chunk: (String,i64,Vec<u8>)=c.db.query_row("SELECT b.hash,b.length,b.compressed FROM migration_evidence_chunks c JOIN migration_evidence_blobs b ON b.hash=c.hash WHERE c.evidence=?1 AND c.offset=0",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    assert_eq!(usize::try_from(chunk.1)?, expected.len());
    assert_eq!(chunk.0, blake3::hash(&expected).to_hex().as_str());
    let mut decoded = Vec::new();
    flate2::read::ZlibDecoder::new(chunk.2.as_slice())
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut decoded)?;
    assert_eq!(decoded, expected);
    drop(source);
    fs::rename(&f.path, f.path.with_extension("offline"))?;
    let before = backup_snapshot::logical_snapshot(&root)?;
    let bundle = temp.path().join("bundle");
    let restored = temp.path().join("restored");
    let limits = Limits {
        min_free_bytes: 0,
        ..Default::default()
    };
    backup_catalog(&root, &bundle, &limits, |_| Ok(()))?;
    restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    assert_eq!(backup_snapshot::logical_snapshot(&restored)?, before);
    let restored = Catalog::open(&restored)?;
    assert_eq!(restored.migration_retention_progress(&input)?, progress);
    assert_eq!(restored.migration_evidence(&id)?, evidence);
    assert!(restored.migration_evidence_chunk(&id, 0).is_err());
    let copied_chunk:(String,i64,Vec<u8>)=restored.db.query_row("SELECT b.hash,b.length,b.compressed FROM migration_evidence_chunks c JOIN migration_evidence_blobs b ON b.hash=c.hash WHERE c.evidence=?1 AND c.offset=0",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    assert_eq!(copied_chunk, chunk);
    assert!(!f.path.exists());
    assert_eq!(backup_snapshot::logical_snapshot(&root)?, before);
    Ok(())
}
