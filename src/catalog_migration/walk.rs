//! Bounded walks between selected retained records and their sealed source.
//! These helpers prepare proofs only; projection remains the coordinator's job.
use super::{
    images,
    lookup::Lookup,
    organization::{Evidence, Link, SourceRecord, verify_unique_link},
    originals::SourceKey,
    retention,
};
use crate::{
    Catalog,
    lightroom::{
        migration_source::{Collection, EvidenceRecord, MigrationSource, Resolution},
        plan::Cell,
    },
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

pub(crate) enum LinkResolution {
    Missing,
    Ambiguous,
    Unavailable { reason: String },
    Unique(Box<Link>),
}

pub(crate) struct Walk<'a> {
    catalog: &'a Catalog,
    source: &'a MigrationSource,
    revision: &'a str,
}
impl<'a> Walk<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        source: &'a MigrationSource,
        revision: &'a str,
    ) -> Result<Self> {
        ensure!(
            source
                .seal()
                .selected
                .iter()
                .any(|s| s.revision == revision)
                && !source
                    .seal()
                    .excluded_revisions
                    .iter()
                    .any(|r| r == revision),
            "walk revision is not selected"
        );
        catalog.migration_retention_progress(source.binding_blake3())?;
        Ok(Self {
            catalog,
            source,
            revision,
        })
    }
    fn require_input(&self, sequence: i64) -> Result<()> {
        let (input, revision, complete): (String, String, bool) = self.catalog.db.query_row(
            "SELECT input,revision,complete FROM migration_retained_records WHERE sequence=?1",
            [sequence],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            input == self.source.binding_blake3() && revision == self.revision && complete,
            "walk record belongs to another input/revision or is incomplete"
        );
        Ok(())
    }
    fn record(&self, sequence: i64) -> Result<EvidenceRecord> {
        self.require_input(sequence)?;
        self.catalog.migration_lookup_record(sequence)
    }
    pub(crate) fn source_record(&self, sequence: i64) -> Result<SourceRecord> {
        let record = self.record(sequence)?;
        ensure!(
            record.collection == Collection::Rows,
            "walk origin is not a retained row"
        );
        let source = SourceKey {
            capture_revision: self.revision.into(),
            table: String::from_utf8(retention::field_bytes(
                &self.catalog.db,
                sequence,
                &record,
                "table_name",
                1024,
            )?)?,
            key: serde_json::from_slice(&retention::field_bytes(
                &self.catalog.db,
                sequence,
                &record,
                "key_json",
                65536,
            )?)?,
        };
        source.identity()?;
        let origin = SourceRecord {
            retained_record: sequence,
            source,
        };
        self.source_id(&origin)?;
        Ok(origin)
    }
    pub(crate) fn source_id(&self, origin: &SourceRecord) -> Result<String> {
        self.require_input(origin.retained_record)?;
        ensure!(
            origin.source.capture_revision == self.revision,
            "walk origin revision differs"
        );
        let mut proof = Evidence::default();
        proof.source(&self.catalog.db, origin)
    }
    /// Only fully interpretable indexed coverage can establish absence here.
    /// Use link() for explicitly typed missing/ambiguous/unavailable outcomes.
    pub(crate) fn singleton(&self, query: &Lookup) -> Result<Option<i64>> {
        let page = self.catalog.migration_lookup(
            self.source.binding_blake3(),
            self.revision,
            query,
            None,
            2,
        )?;
        ensure!(
            page.coverage_complete && page.keys_complete,
            "walk lookup keys unavailable; absence/uniqueness is unproven"
        );
        ensure!(
            page.next.is_none() && page.records.len() <= 1,
            "walk lookup is ambiguous"
        );
        let id = page.records.first().map(|r| r.sequence);
        if let Some(id) = id {
            self.record(id)?;
        }
        Ok(id)
    }
    pub(crate) fn schema(&self, table: &str) -> Result<i64> {
        ensure!(
            !table.is_empty() && table.len() <= 1024 && !table.contains('\0'),
            "walk table bounds"
        );
        let id = self
            .singleton(&Lookup::TableByName(table.into()))?
            .context("retained table schema missing")?;
        let record = self.record(id)?;
        ensure!(
            record.collection == Collection::Tables && inline(&record, "name")? == table,
            "retained schema name differs"
        );
        Ok(id)
    }
    pub(crate) fn columns(&self, origin: &SourceRecord) -> Result<BTreeMap<String, Cell>> {
        self.source_id(origin)?;
        let table = self.schema(&origin.source.table)?;
        images::columns(&self.catalog.db, &mut Evidence::default(), origin, table)
    }
    pub(crate) fn link(
        &self,
        origin: &SourceRecord,
        field: &str,
        target_table: &str,
    ) -> Result<LinkResolution> {
        let source_id = self.source_id(origin)?;
        ensure!(
            [field, target_table]
                .iter()
                .all(|v| !v.is_empty() && v.len() <= 1024 && !v.contains('\0')),
            "walk link bounds"
        );
        let references = self.catalog.migration_lookup(
            self.source.binding_blake3(),
            self.revision,
            &Lookup::References {
                source_id: source_id.clone(),
                field: Some(field.into()),
                target_table: Some(target_table.into()),
            },
            None,
            100,
        )?;
        let resolution = self
            .source
            .resolve(self.revision, &source_id, field, target_table)?;
        let target_id = match resolution {
            Resolution::Ambiguous => return Ok(LinkResolution::Ambiguous),
            Resolution::Missing => {
                return Ok(if references.keys_complete {
                    LinkResolution::Missing
                } else {
                    unavailable(
                        "retained reference keys unavailable; no complete missing-link proof",
                    )
                });
            }
            Resolution::Unique(id) => id,
        };
        // Leave room for origin, target entity and target raw row in the
        // existing shared proof budget, rather than reading an unbounded tail.
        if references.next.is_some() || references.records.len() > 97 {
            return Ok(unavailable(
                "matching references exceed the 100-record proof bound",
            ));
        }
        if target_id.len() > 4096 {
            return Ok(unavailable(
                "live target source ID exceeds lookup key bound",
            ));
        }
        // A live unique resolution does not authorize choosing the first retained
        // endpoint. Duplicate/missing retained IDs must remain unprojected.
        let entities = self.catalog.migration_lookup(
            self.source.binding_blake3(),
            self.revision,
            &Lookup::EntitiesBySource(target_id.clone()),
            None,
            2,
        )?;
        let rows = self.catalog.migration_lookup(
            self.source.binding_blake3(),
            self.revision,
            &Lookup::RowsBySource(target_id.clone()),
            None,
            2,
        )?;
        if entities.records.len() != 1
            || entities.next.is_some()
            || rows.records.len() != 1
            || rows.next.is_some()
        {
            return Ok(unavailable(
                "live unique target lacks unique retained entity/raw-row evidence",
            ));
        }
        let entity_id = entities.records[0].sequence;
        let entity = self.record(entity_id)?;
        if !entities.records[0].unavailable.is_empty() {
            return Ok(unavailable("retained target entity key unavailable"));
        }
        ensure!(
            inline(&entity, "table_name")? == target_table
                && inline(&entity, "source_id")? == target_id,
            "retained target association differs"
        );
        let target_key = inline(&entity, "local_key")?;
        let target = self.source_record(rows.records[0].sequence)?;
        ensure!(
            target.source.table == target_table && self.source_id(&target)? == target_id,
            "retained target row differs"
        );
        // Charge distinct retained payload lengths before decoding references;
        // organization::Evidence enforces the shared 100-record / 8MiB proof cap.
        let mut proof = Evidence::default();
        proof.source(&self.catalog.db, origin)?;
        proof.record(&self.catalog.db, entity_id)?;
        proof.source(&self.catalog.db, &target)?;
        let mut matching = None;
        for hit in references.records {
            self.require_input(hit.sequence)?;
            let reference = proof.record(&self.catalog.db, hit.sequence)?;
            if !hit.unavailable.is_empty() {
                return Ok(unavailable("retained reference key unavailable"));
            }
            ensure!(
                inline(&reference, "source_id")? == source_id
                    && inline(&reference, "field")? == field
                    && inline(&reference, "target_table")? == target_table,
                "retained reference association differs"
            );
            if inline(&reference, "target_key")? == target_key {
                if matching.is_some() {
                    return Ok(unavailable(
                        "live unique target has duplicate retained reference proof",
                    ));
                }
                matching = Some(hit.sequence);
            }
        }
        let Some(reference_record) = matching else {
            return Ok(unavailable(
                "live unique target lacks retained matching reference proof",
            ));
        };
        let link = Link {
            reference_record,
            target_entity_record: entity_id,
            field: field.into(),
            target,
        };
        verify_unique_link(&self.catalog.db, &mut proof, origin, &link, self.source)?;
        Ok(LinkResolution::Unique(Box::new(link)))
    }
}
fn inline<'a>(record: &'a EvidenceRecord, field: &str) -> Result<&'a str> {
    let value = record
        .fields
        .get(field)
        .context("retained lookup field missing")?
        .text()?;
    ensure!(
        value.len() <= 4096,
        "retained lookup field exceeds inline key bound"
    );
    Ok(value)
}
fn unavailable(reason: &str) -> LinkResolution {
    LinkResolution::Unavailable {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightroom::migration_source::{ReadLimits, tests::Fixture};
    use rusqlite::params;

    const APPROVAL: &[u8] = b"synthetic selected indexed walk";
    struct Test {
        _fixture: Fixture,
        _temp: tempfile::TempDir,
        source: MigrationSource,
        catalog: Catalog,
        revision: String,
    }
    impl Test {
        fn new(mode: &str) -> Result<Self> {
            let mut fixture = Fixture::new();
            let revision = fixture.revision().to_owned();
            fixture.seal.approval.document_blake3 = blake3::hash(APPROVAL).to_hex().to_string();
            fixture.edit(|db|{
                for (name,columns) in [("walk_child",vec!["id","parent"]),("walk_parent",vec!["id"])] {
                    db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,?3,'[]','{}','fixture',1,1,'complete')",params![revision,name,serde_json::to_string(&columns).unwrap()]).unwrap();
                }
                let local_key=if mode=="oversized" {serde_json::to_string(&Cell::Text(vec![b'x';3000])).unwrap()}else{serde_json::to_string(&Cell::Integer(17913)).unwrap()};
                for (id,table,key,cells) in [("child","walk_child",1,vec![Cell::Integer(1),Cell::RealBits(17913.0f64.to_bits())]),("parent","walk_parent",17913,vec![Cell::Integer(17913)])] {
                    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,?5)",params![revision,id,table,serde_json::to_string(&vec![Cell::Integer(key)]).unwrap(),serde_json::to_string(&cells).unwrap()]).unwrap();
                    let key=if id=="parent" {local_key.clone()}else{serde_json::to_string(&Cell::Integer(1)).unwrap()};
                    db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,id,table,key]).unwrap();
                }
                let target=if mode=="numeric_mismatch" {serde_json::to_string(&Cell::RealBits(17913.0f64.to_bits())).unwrap()}else{local_key.clone()};
                db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,'child','parent','walk_parent',?2)",params![revision,target]).unwrap();
                if mode=="extra_dangling" {
                    db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,'child','parent','walk_parent','no-such-literal-key')",[&revision]).unwrap();
                }
                if mode=="too_many_refs" {
                    for i in 0..98 {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,'child','parent','walk_parent',?2)",params![revision,format!("missing-{i}")]).unwrap();
                    }
                }
                if mode=="duplicate_entity" {
                    db.execute("INSERT INTO entities VALUES(?1,'other-parent','walk_parent',?2,NULL,'{}')",params![revision,local_key]).unwrap();
                }
                if mode=="duplicate_row" {
                    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,'parent','walk_parent',?2,?3)",params![revision,serde_json::to_string(&vec![Cell::Integer(2)]).unwrap(),serde_json::to_string(&vec![Cell::Integer(2)]).unwrap()]).unwrap();
                }
            });
            let source = fixture.open();
            let temp = tempfile::tempdir()?;
            let mut catalog = Catalog::open(temp.path().join("catalog"))?;
            catalog.begin_migration_retention(&source, APPROVAL)?;
            for _ in 0..1000 {
                if catalog.step_migration_retention(&source)?.complete {
                    break;
                }
            }
            ensure!(
                catalog
                    .migration_retention_progress(source.binding_blake3())?
                    .complete,
                "fixture custody incomplete"
            );
            Ok(Self {
                _fixture: fixture,
                _temp: temp,
                source,
                catalog,
                revision,
            })
        }
        fn walk(&self) -> Result<Walk<'_>> {
            Walk::new(&self.catalog, &self.source, &self.revision)
        }
        fn child(&self) -> Result<SourceRecord> {
            let walk = self.walk()?;
            walk.source_record(
                walk.singleton(&Lookup::RowsBySource("child".into()))?
                    .context("fixture child missing")?,
            )
        }
    }
    #[test]
    fn exact_columns_and_unique_link_use_retained_ids_and_live_truth() -> Result<()> {
        for mode in ["unique", "extra_dangling"] {
            let t = Test::new(mode)?;
            let walk = t.walk()?;
            let child = t.child()?;
            assert_eq!(walk.source_id(&child)?, "child");
            assert_eq!(child.source.key, vec![Cell::Integer(1)]);
            let columns = walk.columns(&child)?;
            assert_eq!(columns["parent"], Cell::RealBits(17913.0f64.to_bits()));
            let LinkResolution::Unique(link) = walk.link(&child, "parent", "walk_parent")? else {
                anyhow::bail!("expected unique literal retained link")
            };
            assert_eq!(link.target.source.key, vec![Cell::Integer(17913)]);
            assert_eq!(walk.source_id(&link.target)?, "parent");
            assert!(matches!(
                walk.link(&child, "not-a-field", "walk_parent")?,
                LinkResolution::Missing
            ));
            assert!(walk.schema("missing table").is_err());
            let entity = walk
                .singleton(&Lookup::EntitiesBySource("parent".into()))?
                .unwrap();
            assert!(walk.source_record(entity).is_err());
        }
        Ok(())
    }
    #[test]
    fn duplicate_ids_and_oversized_keys_never_pick_first_or_prove_missing() -> Result<()> {
        let t = Test::new("duplicate_entity")?;
        assert!(matches!(
            t.walk()?.link(&t.child()?, "parent", "walk_parent")?,
            LinkResolution::Ambiguous
        ));
        let t = Test::new("duplicate_row")?;
        assert!(matches!(
            t.walk()?.link(&t.child()?, "parent", "walk_parent")?,
            LinkResolution::Unavailable { .. }
        ));
        assert!(
            t.walk()?
                .singleton(&Lookup::RowsBySource("parent".into()))
                .is_err()
        );
        let t = Test::new("too_many_refs")?;
        assert!(matches!(
            t.source
                .resolve(&t.revision, "child", "parent", "walk_parent")?,
            Resolution::Unique(_)
        ));
        assert!(matches!(
            t.walk()?.link(&t.child()?, "parent", "walk_parent")?,
            LinkResolution::Unavailable { .. }
        ));
        let t = Test::new("oversized")?;
        assert!(matches!(
            t.source
                .resolve(&t.revision, "child", "parent", "walk_parent")?,
            Resolution::Unique(_)
        ));
        match t.walk()?.link(&t.child()?, "parent", "walk_parent")? {
            LinkResolution::Unavailable { reason } => assert!(reason.contains("key unavailable")),
            _ => anyhow::bail!("oversized key acquired interpreted link"),
        }
        assert!(matches!(
            t.walk()?.link(&t.child()?, "not-a-field", "walk_parent")?,
            LinkResolution::Unavailable { .. }
        ));
        Ok(())
    }
    #[test]
    fn numeric_key_strings_and_original_typed_keys_are_not_recanonicalized() -> Result<()> {
        let t = Test::new("numeric_mismatch")?;
        let walk = t.walk()?;
        let mut child = t.child()?;
        assert!(matches!(
            walk.link(&child, "parent", "walk_parent")?,
            LinkResolution::Missing
        ));
        child.source.key = vec![Cell::RealBits(1.0f64.to_bits())];
        assert!(walk.source_id(&child).is_err());
        assert!(walk.columns(&child).is_err());
        assert!(walk.link(&child, "parent", "walk_parent").is_err());
        Ok(())
    }
    #[test]
    fn excluded_and_cross_input_origins_are_rejected_before_source_resolution() -> Result<()> {
        let mut t = Test::new("unique")?;
        let child = t.child()?;
        assert!(
            Walk::new(
                &t.catalog,
                &t.source,
                &t.source.seal().excluded_revisions[0]
            )
            .is_err()
        );
        let mut wrong = child.clone();
        wrong.source.capture_revision = t.source.seal().excluded_revisions[0].clone();
        assert!(t.walk()?.source_id(&wrong).is_err());
        let other_approval = b"separate synthetic approval";
        let mut seal = t.source.seal().clone();
        seal.approval.document_blake3 = blake3::hash(other_approval).to_hex().to_string();
        let other = MigrationSource::open(seal, ReadLimits::default())?;
        t.catalog
            .begin_migration_retention(&other, other_approval)?;
        let walk = Walk::new(&t.catalog, &other, &t.revision)?;
        assert!(walk.source_record(child.retained_record).is_err());
        assert!(walk.source_id(&child).is_err());
        assert!(walk.columns(&child).is_err());
        assert!(walk.link(&child, "parent", "walk_parent").is_err());
        Ok(())
    }
    #[test]
    fn changed_sealed_source_cannot_supply_a_new_unique_link() -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let t = Test::new("unique")?;
        let child = t.child()?;
        let path = &t._fixture.path;
        let offset = t.source.seal().identity.bytes - 1;
        let mut before = std::fs::File::open(path)?;
        before.seek(SeekFrom::Start(offset))?;
        let mut byte = [0u8];
        before.read_exact(&mut byte)?;
        drop(before);
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(mut file) => {
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&[byte[0] ^ 1])?;
                file.sync_all()?;
                drop(file);
                assert!(t.walk()?.link(&child, "parent", "walk_parent").is_err());
            }
            Err(error) => {
                #[cfg(not(windows))]
                return Err(error.into());
                #[cfg(windows)]
                {
                    assert_eq!(error.raw_os_error(), Some(32));
                    assert!(matches!(
                        t.walk()?.link(&child, "parent", "walk_parent")?,
                        LinkResolution::Unique(_)
                    ));
                }
            }
        }
        Ok(())
    }
}
