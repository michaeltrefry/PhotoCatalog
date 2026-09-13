//! Uses the genuine legacy-repair fixture, including source seals and old
//! Retain receipts. No domain rows are invented for the backup oracle.
use super::super::backup_snapshot;
use super::*;
use crate::catalog_backup::{Limits, backup_catalog, restore_catalog, restore_status};

#[test]
fn backup_preserves_all_ten_keyword_repair_phases_and_exact_predecessors() -> Result<()> {
    let f = fixture()?;
    let (mut c, s, run, current) = completed(&f)?;
    let key = f.key(&c, 0, 21)?;
    let metadata = c.metadata_for_image(&key)?;
    c.organize_image(
        &key,
        metadata.revision,
        crate::organization::Operation::AddKeyword {
            kind: crate::organization::KeywordKind::Hierarchical,
            path: vec!["Local choice".into()],
        },
    )?;
    let request = request(&c, &s, &run, &current)?;
    let repair = c.begin_keyword_repair(&s, &request)?;
    let copies = tempfile::tempdir()?;
    let limits = Limits {
        min_free_bytes: 0,
        ..Default::default()
    };
    let mut phases = std::collections::BTreeSet::new();
    let mut checked_partial_reports = false;
    let mut checked_membership = false;
    for step in 0..160 {
        let progress = c.keyword_repair_progress(&repair.id)?;
        let phase = format!("{:?}", progress.phase);
        let first = phases.insert(phase);
        let partial_reports:bool=c.db.query_row("SELECT EXISTS(SELECT 1 FROM migration_keyword_repair_reports WHERE new_digest IS NULL)",[],|r|r.get(0))?;
        let membership:Option<i64>=c.db.query_row("SELECT min(record) FROM migration_keyword_repair_items WHERE repair=?1 AND stage='\"KeywordMemberships\"' AND new_outcome_digest IS NOT NULL",[&repair.id],|r|r.get(0))?;
        if first
            || (partial_reports && !checked_partial_reports)
            || (membership.is_some() && !checked_membership)
        {
            let before = backup_snapshot::logical_snapshot(&f.destination)?;
            let bundle = copies.path().join(format!("bundle-{step}"));
            let restored = copies.path().join(format!("restored-{step}"));
            backup_catalog(&f.destination, &bundle, &limits, |_| Ok(()))?;
            restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
            assert_eq!(
                backup_snapshot::logical_snapshot(&restored)?,
                before,
                "phase {:?}",
                progress.phase
            );
            assert_eq!(backup_snapshot::logical_snapshot(&bundle)?, before);
            assert_eq!(backup_snapshot::logical_snapshot(&f.destination)?, before);
            let restored = Catalog::open(&restored)?;
            assert_eq!(
                serde_json::to_value(restored.keyword_repair_progress(&repair.id)?)?,
                serde_json::to_value(&progress)?
            );
            assert_eq!(
                serde_json::to_value(restored.current_develop_repair_progress(&current)?)?,
                serde_json::to_value(c.current_develop_repair_progress(&current)?)?
            );
            assert_eq!(
                serde_json::to_value(restored.metadata_for_image(&key)?)?,
                serde_json::to_value(c.metadata_for_image(&key)?)?
            );
            assert_eq!(
                serde_json::to_value(restored.edit_variant(&key)?)?,
                serde_json::to_value(c.edit_variant(&key)?)?
            );
            assert!(restore_status(&restored.root)?.unwrap().jobs_held);
            if let Some(record) = membership {
                let old =
                    c.keyword_repair_predecessor(&repair.id, Stage::KeywordMemberships, record)?;
                assert!(old.planned_decision.is_none());
                assert!(old.old_receipt.is_none());
                assert_eq!(
                    serde_json::to_value(restored.keyword_repair_predecessor(
                        &repair.id,
                        Stage::KeywordMemberships,
                        record
                    )?)?,
                    serde_json::to_value(old)?
                );
                checked_membership = true;
            }
            checked_partial_reports |= partial_reports;
        }
        if progress.complete {
            break;
        }
        c.step_keyword_repair(&s, &repair.id)?;
    }
    assert_eq!(
        phases,
        [
            "ArchiveReports",
            "PlanDictionaries",
            "PlanMemberships",
            "Order",
            "Dictionaries",
            "Memberships",
            "VerifyDictionaries",
            "VerifyMemberships",
            "Reconciliation",
            "Complete"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
    assert!(checked_partial_reports && checked_membership);
    assert!(c.keyword_repair_progress(&repair.id)?.complete);
    Ok(())
}
