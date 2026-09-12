//! Selected Lightroom image projection. Files and logical variants remain distinct.
use super::{
    organization::{Evidence, Link, SourceRecord, verify_unique_link},
    retention,
};
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_images::{self, ImageRole, ImportImageRequest},
    catalog_writer::Priority,
    lightroom::{
        migration_source::{Collection, MigrationSource},
        plan::Cell,
    },
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const LIMIT: usize = 8 * 1024 * 1024;
pub const ADAPTER: &str = "lightroom-selected-images-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Role {
    /// The retained masterImage column must contain NULL or numeric zero.
    Master,
    Virtual {
        master: Link,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Decision {
    Register {
        file: Box<Link>,
        role: Role,
        label: String,
    },
    /// A source with an unresolved file/parent is retained without inventing a
    /// filesystem location or a native parent. The reconciler counts this gap.
    Retain { reason: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub origin: SourceRecord,
    pub retained_table: i64,
    pub import_source: String,
    pub decision: Decision,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Outcome {
    Image { id: String, key: VariantKey },
    Retained { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionResult {
    pub source_identity: String,
    pub input_digest: String,
    pub outcome: Outcome,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration_images(
        source_identity TEXT PRIMARY KEY, owner TEXT NOT NULL, input_digest TEXT NOT NULL,
        retained_record INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        retained_table INTEGER NOT NULL REFERENCES migration_retained_records(sequence),
        result TEXT NOT NULL);",
    )?;
    Ok(())
}

/// Interpret one selected row using its retained original column roster.
/// Unknown columns are left in custody; no derived entity JSON replaces raw cells.
pub(crate) fn table_proof(
    db: &Connection,
    evidence: &mut Evidence,
    origin: &SourceRecord,
    table: i64,
) -> Result<()> {
    evidence.source(db, origin)?;
    let schema = evidence.record(db, table)?;
    evidence.same_input(origin.retained_record, table)?;
    ensure!(
        schema.collection == Collection::Tables
            && schema.revision == origin.source.capture_revision,
        "source columns require the same selected capture"
    );
    ensure!(
        retention::field_bytes(db, table, &schema, "name", 1024)? == origin.source.table.as_bytes(),
        "source table proof differs"
    );
    Ok(())
}
pub(crate) fn columns(
    db: &Connection,
    evidence: &mut Evidence,
    origin: &SourceRecord,
    table: i64,
) -> Result<std::collections::BTreeMap<String, Cell>> {
    table_proof(db, evidence, origin, table)?;
    let row = evidence.record(db, origin.retained_record)?;
    let schema = evidence.record(db, table)?;
    let names: Vec<String> = serde_json::from_slice(&retention::field_bytes(
        db,
        table,
        &schema,
        "columns_json",
        LIMIT,
    )?)?;
    let values: Vec<Cell> = serde_json::from_slice(&retention::field_bytes(
        db,
        origin.retained_record,
        &row,
        "cells_json",
        LIMIT,
    )?)?;
    ensure!(
        names.len() == values.len() && names.len() <= 4096,
        "source column/value roster differs"
    );
    let count = names.len();
    let fields: std::collections::BTreeMap<_, _> = names.into_iter().zip(values).collect();
    ensure!(fields.len() == count, "duplicate source column name");
    Ok(fields)
}
fn zero_or_null(cell: &Cell) -> bool {
    match cell {
        Cell::Null => true,
        Cell::Integer(v) => *v == 0,
        Cell::RealBits(v) => f64::from_bits(*v) == 0.0,
        _ => false,
    }
}
fn digest(request: &Projection) -> Result<String> {
    request.origin.source.identity()?;
    match &request.decision {
        Decision::Retain { reason } => ensure!(
            !reason.trim().is_empty() && reason.len() <= 16384,
            "retained image requires a bounded reason"
        ),
        Decision::Register { file, role, label } => {
            ensure!(
                !label.is_empty() && label.len() <= 4096,
                "image label bounds"
            );
            file.target.source.identity()?;
            if let Role::Virtual { master } = role {
                master.target.source.identity()?;
            }
        }
    }
    // Evidence sequence numbers and inspection lineage can change on a second
    // sealed inspection; immutable capture + original keys carry identity.
    let decision = match &request.decision {
        Decision::Retain { reason } => serde_json::json!({"retain":reason}),
        Decision::Register { file, role, label } => {
            serde_json::json!({"file":file.target.source,"label":label,"role":match role {
                Role::Master=>serde_json::json!({"master":true}),
                Role::Virtual{master}=>serde_json::json!({"virtual":master.target.source}),
            }})
        }
    };
    let bytes = serde_json::to_vec(
        &serde_json::json!({"adapter":ADAPTER,"owner":request.import_source,"source":request.origin.source,"decision":decision}),
    )?;
    ensure!(bytes.len() <= 65536, "image projection request bounds");
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
fn existing(
    db: &Connection,
    identity: &str,
    owner: &str,
    digest: &str,
) -> Result<Option<ProjectionResult>> {
    let found: Option<(String, String, String)> = db
        .query_row(
            "SELECT owner,input_digest,result FROM migration_images WHERE source_identity=?",
            [identity],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    found
        .map(|(old_owner, old_digest, result)| {
            ensure!(
                old_owner == owner && old_digest == digest,
                "source image decision changed; explicit reconciliation required"
            );
            ensure!(result.len() <= 65536, "image result bounds");
            Ok(serde_json::from_str(&result)?)
        })
        .transpose()
}
pub(crate) fn mapped_image(
    db: &Connection,
    owner: &str,
    source: &super::originals::SourceKey,
) -> Result<VariantKey> {
    let identity = source.identity()?;
    db.query_row("SELECT i.asset_id,i.variant_id FROM image_import_map m JOIN catalog_images i ON i.id=m.image_id WHERE m.import_source=?1 AND m.capture_revision=?2 AND m.source_table=?3 AND m.source_id=?4",params![owner,source.capture_revision,source.table,identity],|r|Ok(VariantKey{asset_id:r.get(0)?,variant_id:r.get(1)?})).context("selected image has no native mapping")
}
impl Catalog {
    pub fn project_migration_image(
        &mut self,
        source: Option<&MigrationSource>,
        request: &Projection,
    ) -> Result<ProjectionResult> {
        ensure!(
            !request.import_source.is_empty()
                && request.import_source.len() <= 4096
                && !request.import_source.contains('\0'),
            "image import owner bounds"
        );
        ensure!(
            request.origin.source.table == "Adobe_images",
            "image projection requires Adobe_images"
        );
        let identity = request.origin.source.identity()?;
        let digest = digest(request)?;
        let mut proof = Evidence::default();
        table_proof(
            &self.db,
            &mut proof,
            &request.origin,
            request.retained_table,
        )?;
        if let Some(result) = existing(&self.db, &identity, &request.import_source, &digest)? {
            return Ok(result);
        }
        let mut resolved = None;
        if let Decision::Register { file, role, label } = &request.decision {
            let fields = columns(
                &self.db,
                &mut proof,
                &request.origin,
                request.retained_table,
            )?;
            ensure!(
                !label.is_empty() && label.len() <= 4096,
                "image label bounds"
            );
            ensure!(
                file.field == "rootFile" && file.target.source.table == "AgLibraryFile",
                "image file link differs"
            );
            let source = source.context("first image projection requires the sealed source")?;
            verify_unique_link(&self.db, &mut proof, &request.origin, file, source)?;
            let file_identity = file.target.source.identity()?;
            let (asset,owner):(String,String)=self.db.query_row("SELECT asset_id,import_source FROM migration_originals WHERE source_identity=?",[&file_identity],|r|Ok((r.get(0)?,r.get(1)?))).context("file must be explicitly registered before image")?;
            ensure!(
                owner == request.import_source,
                "file belongs to a different importer"
            );
            let (role, parent) = match role {
                Role::Master => {
                    ensure!(
                        fields.get("masterImage").is_some_and(zero_or_null),
                        "source row does not prove a master sentinel"
                    );
                    (ImageRole::Master, None)
                }
                Role::Virtual { master } => {
                    ensure!(
                        master.field == "masterImage"
                            && master.target.source.table == "Adobe_images",
                        "virtual parent link differs"
                    );
                    ensure!(
                        fields.get("masterImage").is_some_and(|v| !zero_or_null(v)),
                        "virtual source has no master reference"
                    );
                    verify_unique_link(&self.db, &mut proof, &request.origin, master, source)?;
                    (
                        ImageRole::Virtual,
                        Some(mapped_image(
                            &self.db,
                            &request.import_source,
                            &master.target.source,
                        )?),
                    )
                }
            };
            resolved = Some((file_identity, asset, role, parent, label.clone()));
        } else if let Decision::Retain { reason } = &request.decision {
            ensure!(
                !reason.trim().is_empty() && reason.len() <= 16384,
                "retained image requires a bounded reason"
            );
        }
        let _permit = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        proof.recheck(&tx)?;
        if let Some(result) = existing(&tx, &identity, &request.import_source, &digest)? {
            tx.commit()?;
            return Ok(result);
        }
        let outcome = if let Some((file_identity, asset, role, parent, label)) = resolved {
            ensure!(tx.query_row("SELECT asset_id=?2 AND import_source=?3 FROM migration_originals WHERE source_identity=?1",params![file_identity,asset,request.import_source],|r|r.get::<_,bool>(0))?,"original mapping changed");
            if let (
                Decision::Register {
                    role: Role::Virtual { master },
                    ..
                },
                Some(parent),
            ) = (&request.decision, &parent)
            {
                ensure!(
                    mapped_image(&tx, &request.import_source, &master.target.source)? == *parent,
                    "virtual parent mapping changed"
                );
            }
            let reserved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM image_import_reservations WHERE asset_id=?1 AND owner=?2)",params![asset,request.import_source],|r|r.get(0))?;
            let image = catalog_images::register_import_image(
                &tx,
                &ImportImageRequest {
                    import_source: request.import_source.clone(),
                    capture_revision: request.origin.source.capture_revision.clone(),
                    source_table: request.origin.source.table.clone(),
                    source_id: identity.clone(),
                    input_digest: digest.clone(),
                    adapter_version: ADAPTER.into(),
                    asset_id: asset,
                    claim_reserved_master: role == ImageRole::Master && reserved,
                    role,
                    master: parent,
                    label,
                },
            )?;
            Outcome::Image {
                id: image.id,
                key: image.key,
            }
        } else {
            let Decision::Retain { reason } = &request.decision else {
                unreachable!()
            };
            Outcome::Retained {
                reason: reason.clone(),
            }
        };
        let result = ProjectionResult {
            source_identity: identity.clone(),
            input_digest: digest.clone(),
            outcome,
        };
        tx.execute(
            "INSERT INTO migration_images VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                identity,
                request.import_source,
                digest,
                request.origin.retained_record,
                request.retained_table,
                serde_json::to_string(&result)?
            ],
        )?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog_migration::originals::{OriginalDecision, OriginalRequest, SourceKey},
        lightroom::migration_source::{EvidenceRecord, tests::Fixture},
        storage_volume::NativePath,
    };
    use std::collections::BTreeMap;

    struct Test {
        _fixture: Fixture,
        _temp: tempfile::TempDir,
        source: MigrationSource,
        catalog: Catalog,
        rows: BTreeMap<i64, SourceRecord>,
        tables: BTreeMap<String, i64>,
        references: BTreeMap<(String, String), i64>,
        entities: BTreeMap<String, i64>,
    }
    impl Test {
        fn new() -> Result<Self> {
            Self::with_oversized(false)
        }
        fn with_oversized(oversized: bool) -> Result<Self> {
            Self::with_oversized_parts(oversized, oversized)
        }
        fn with_oversized_parts(oversized: bool, oversized_packets: bool) -> Result<Self> {
            let mut fixture = Fixture::new();
            let revision = fixture.revision().to_owned();
            let approval = b"approved synthetic logical image import";
            fixture.seal.approval.document_blake3 = blake3::hash(approval).to_hex().to_string();
            fixture.edit(|db| {
                let definitions=[
                    ("AgLibraryFile",vec!["id_local"]),
                    ("Adobe_images",vec!["id_local","rootFile","masterImage","fileFormat","developSettingsIDCache"]),
                    ("Adobe_imageDevelopSettings",vec!["id_local","text"]),
                    ("Adobe_AdditionalMetadata",vec!["id_local","image","xmp"]),
                ];
                for (table,columns) in definitions {
                    db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?1,?2,?3,'[]','{}','fixture',1,1,'complete')",params![revision,table,serde_json::to_string(&columns).unwrap()]).unwrap();
                }
                let xmp=br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:unknown" xmp:Rating="4"><u:opaque>preserve exactly</u:opaque></rdf:Description></rdf:RDF>"#;
                use std::io::Write;
                let mut encoder=flate2::write::ZlibEncoder::new(Vec::new(),flate2::Compression::fast());
                encoder.write_all(xmp).unwrap();
                let mut raw=(xmp.len() as u32).to_be_bytes().to_vec();
                raw.extend(encoder.finish().unwrap());
                for (id,table,mut cells) in [
                    (10,"AgLibraryFile",vec![Cell::Integer(10)]),
                    (20,"Adobe_images",vec![Cell::Integer(20),Cell::Integer(10),Cell::Null,Cell::Text(b"RAW".to_vec()),Cell::Integer(30)]),
                    (21,"Adobe_images",vec![Cell::Integer(21),Cell::RealBits(10.0f64.to_bits()),Cell::RealBits(20.0f64.to_bits()),Cell::Text(b"RAW".to_vec()),Cell::Integer(31)]),
                    (22,"Adobe_images",vec![Cell::Integer(22),Cell::Integer(10),Cell::Integer(0),Cell::Text(b"RAW".to_vec()),Cell::Integer(30)]),
                    (30,"Adobe_imageDevelopSettings",vec![Cell::Integer(30),Cell::Text(b"{ProcessVersion='11.0',Exposure2012=1,UnknownPlugin={opaque='keep'}}".to_vec())]),
                    (31,"Adobe_imageDevelopSettings",vec![Cell::Integer(31),Cell::Text(b"{ProcessVersion='11.0',Exposure2012=-1,Contrast2012=25}".to_vec())]),
                    (40,"Adobe_AdditionalMetadata",vec![Cell::Integer(40),Cell::Integer(20),Cell::Blob(raw.clone())]),
                ] {
                    if oversized && matches!(id,22|30|40) {
                        *cells.last_mut().unwrap()=Cell::Blob(vec![b'a';9*1024*1024]);
                    }
                    let source_id=format!("source-{id}");
                    db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES(?1,?2,?3,?4,?5)",params![revision,source_id,table,serde_json::to_string(&vec![Cell::Integer(id)]).unwrap(),serde_json::to_string(&cells).unwrap()]).unwrap();
                    db.execute("INSERT INTO entities VALUES(?1,?2,?3,?4,NULL,'{}')",params![revision,source_id,table,serde_json::to_string(&Cell::Integer(id)).unwrap()]).unwrap();
                    if table=="Adobe_images" {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,'rootFile','AgLibraryFile',?3)",params![revision,source_id,serde_json::to_string(&Cell::Integer(10)).unwrap()]).unwrap();
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,'developSettingsIDCache','Adobe_imageDevelopSettings',?3)",params![revision,source_id,serde_json::to_string(&Cell::Integer(if id==21 {31}else{30})).unwrap()]).unwrap();
                    }
                    if id==21 {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,'masterImage','Adobe_images',?3)",params![revision,source_id,serde_json::to_string(&Cell::Integer(20)).unwrap()]).unwrap();
                    }
                    if id==40 {
                        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,?2,'image','Adobe_images',?3)",params![revision,source_id,serde_json::to_string(&Cell::Integer(20)).unwrap()]).unwrap();
                        let raw=if oversized_packets {vec![b'a';9*1024*1024]}else{raw.clone()};
                        let decoded=if oversized_packets {vec![b'b';9*1024*1024]}else{xmp.to_vec()};
                        db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?1,?2,'catalog',?3,?4,?5,'fixture exact catalog XMP')",params![revision,source_id,blake3::hash(&raw).to_hex().to_string(),raw,decoded]).unwrap();
                    }
                }
            });
            let source = fixture.open();
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
                "fixture custody incomplete"
            );
            let get = |collection| {
                catalog.retained_migration_records(
                    source.binding_blake3(),
                    &revision,
                    collection,
                    0,
                    100,
                )
            };
            let text = |record: &EvidenceRecord, field: &str| -> Result<String> {
                Ok(record.fields[field].text()?.to_owned())
            };
            let mut rows = BTreeMap::new();
            for (sequence, record) in get(Collection::Rows)? {
                let table = text(&record, "table_name")?;
                if table == "unknown_plugin" {
                    continue;
                }
                let key: Vec<Cell> = serde_json::from_str(&text(&record, "key_json")?)?;
                let Cell::Integer(id) = key[0] else {
                    unreachable!()
                };
                rows.insert(
                    id,
                    SourceRecord {
                        retained_record: sequence,
                        source: SourceKey {
                            capture_revision: revision.clone(),
                            table,
                            key,
                        },
                    },
                );
            }
            let tables = get(Collection::Tables)?
                .into_iter()
                .map(|(s, r)| Ok((text(&r, "name")?, s)))
                .collect::<Result<_>>()?;
            let references = get(Collection::References)?
                .into_iter()
                .map(|(s, r)| Ok(((text(&r, "source_id")?, text(&r, "field")?), s)))
                .collect::<Result<_>>()?;
            let entities = get(Collection::Entities)?
                .into_iter()
                .map(|(s, r)| Ok((text(&r, "source_id")?, s)))
                .collect::<Result<_>>()?;
            Ok(Self {
                _fixture: fixture,
                _temp: temp,
                source,
                catalog,
                rows,
                tables,
                references,
                entities,
            })
        }
        fn link(&self, from: i64, field: &str, to: i64) -> Link {
            Link {
                reference_record: self.references[&(format!("source-{from}"), field.into())],
                target_entity_record: self.entities[&format!("source-{to}")],
                field: field.into(),
                target: self.rows[&to].clone(),
            }
        }
        fn request(&self, id: i64) -> Projection {
            Projection {
                origin: self.rows[&id].clone(),
                retained_table: self.tables["Adobe_images"],
                import_source: "lightroom".into(),
                decision: Decision::Register {
                    file: Box::new(self.link(id, "rootFile", 10)),
                    role: if id == 21 {
                        Role::Virtual {
                            master: self.link(id, "masterImage", 20),
                        }
                    } else {
                        Role::Master
                    },
                    label: format!("source-{id}"),
                },
            }
        }
        fn original(&mut self) -> Result<String> {
            let file = &self.rows[&10];
            Ok(self
                .catalog
                .register_migration_original(&OriginalRequest {
                    import_source: "lightroom".into(),
                    source: file.source.clone(),
                    retained_record: file.retained_record,
                    decision: OriginalDecision::Create {
                        path: NativePath::UnixBytes(b"/missing/2014/January/photo.CR2".to_vec()),
                    },
                })?
                .asset_id)
        }
        fn project(&mut self, id: i64) -> Result<ProjectionResult> {
            self.catalog
                .project_migration_image(Some(&self.source), &self.request(id))
        }
    }
    #[test]
    fn masters_and_virtual_copies_share_file_without_merging_identity() -> Result<()> {
        let mut t = Test::new()?;
        let asset = t.original()?;
        let request = t.request(20);
        let master = t.project(20)?;
        let virtual_copy = t.project(21)?;
        let other_master = t.project(22)?;
        let Outcome::Image {
            id: master_id,
            key: master_key,
        } = &master.outcome
        else {
            panic!("image expected")
        };
        let Outcome::Image {
            id: copy_id,
            key: copy_key,
        } = &virtual_copy.outcome
        else {
            panic!("image expected")
        };
        let Outcome::Image {
            id: other_id,
            key: other_key,
        } = &other_master.outcome
        else {
            panic!("image expected")
        };
        assert_eq!(master_key, &VariantKey::master(&asset));
        assert_eq!(copy_key.asset_id, asset);
        assert_eq!(other_key.asset_id, asset);
        assert_ne!(master_id, copy_id);
        assert_ne!(master_id, other_id);
        assert_ne!(copy_id, other_id);
        assert_eq!(
            t.catalog.image(copy_key)?.master_sequence,
            Some(t.catalog.image(master_key)?.sequence)
        );
        assert_eq!(t.catalog.browse_images(0, 100)?.len(), 3);
        let original_location: Vec<u8> =
            t.catalog
                .db
                .query_row("SELECT location FROM assets WHERE id=?", [asset], |r| {
                    r.get(0)
                })?;
        assert!(!original_location.is_empty());
        // Replay after editing must preserve user state and require no original or adapter.
        let rev = t.catalog.metadata_for_image(copy_key)?.revision;
        t.catalog.edit_metadata_for_image(
            copy_key,
            rev,
            None,
            &[crate::xmp::Edit::Set {
                namespace: crate::xmp::XMP.into(),
                path: "Rating".into(),
                value: "5".into(),
            }],
        )?;
        assert_eq!(t.catalog.project_migration_image(None, &request)?, master);
        assert_eq!(t.project(21)?, virtual_copy);
        assert_eq!(t.catalog.browse_images(0, 100)?.len(), 3);
        Ok(())
    }
    #[test]
    fn forged_relationships_missing_parents_and_changed_decisions_do_not_mutate() -> Result<()> {
        let mut t = Test::new()?;
        t.original()?;
        assert!(t.project(21).is_err());
        let mut request = t.request(20);
        request.retained_table = t.tables["AgLibraryFile"];
        assert!(
            t.catalog
                .project_migration_image(Some(&t.source), &request)
                .is_err()
        );
        request = t.request(21);
        if let Decision::Register { role, .. } = &mut request.decision {
            *role = Role::Master;
        }
        assert!(
            t.catalog
                .project_migration_image(Some(&t.source), &request)
                .is_err()
        );
        let master = t.project(20)?;
        let mut changed = t.request(20);
        if let Decision::Register { label, .. } = &mut changed.decision {
            *label = "changed".into();
        }
        assert!(t.catalog.project_migration_image(None, &changed).is_err());
        assert_eq!(t.project(20)?, master);
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM migration_images", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }
    #[test]
    fn image_checkpoint_failure_rolls_back_reserved_master_claim() -> Result<()> {
        let mut t = Test::new()?;
        t.original()?;
        t.catalog.db.execute_batch("CREATE TRIGGER fail_image_checkpoint BEFORE INSERT ON migration_images BEGIN SELECT RAISE(ABORT,'fixture crash'); END;")?;
        assert!(t.project(20).is_err());
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM image_import_map", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM image_import_reservations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            1
        );
        t.catalog
            .db
            .execute_batch("DROP TRIGGER fail_image_checkpoint;")?;
        t.project(20)?;
        assert_eq!(
            t.catalog
                .db
                .query_row("SELECT count(*) FROM image_import_reservations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            0
        );
        Ok(())
    }
    fn develop(
        t: &Test,
        id: i64,
        settings: i64,
    ) -> crate::catalog_migration::metadata::CurrentDevelop {
        crate::catalog_migration::metadata::CurrentDevelop {
            image: t.rows[&id].clone(),
            image_table: t.tables["Adobe_images"],
            settings: t.link(id, "developSettingsIDCache", settings),
            settings_table: t.tables["Adobe_imageDevelopSettings"],
            settings_path: vec![],
            import_source: "lightroom".into(),
            expected_edit_revision: 0,
        }
    }
    #[test]
    fn current_adobe_settings_are_independent_atomic_and_replay_preserves_user_edits() -> Result<()>
    {
        let mut t = Test::new()?;
        t.original()?;
        t.project(20)?;
        t.project(21)?;
        let master = develop(&t, 20, 30);
        let copy = develop(&t, 21, 31);
        t.catalog.db.execute_batch("CREATE TRIGGER fail_metadata_checkpoint BEFORE INSERT ON migration_metadata BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;")?;
        let master_key = mapped_image(&t.catalog.db, "lightroom", &master.image.source)?;
        assert!(
            t.catalog
                .project_migration_current_develop(Some(&t.source), &master)
                .is_err()
        );
        assert_eq!(t.catalog.edit_variant(&master_key)?.revision, 0);
        t.catalog
            .db
            .execute_batch("DROP TRIGGER fail_metadata_checkpoint;")?;
        let imported = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &master)?;
        let copied = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &copy)?;
        assert_eq!(
            imported
                .extraction
                .as_ref()
                .unwrap()
                .contribution
                .exposure_ev,
            Some(1.0)
        );
        assert_eq!(
            copied.extraction.as_ref().unwrap().contribution.exposure_ev,
            Some(-1.0)
        );
        assert!(
            imported
                .extraction
                .as_ref()
                .unwrap()
                .properties
                .iter()
                .any(|p| p.name == "opaque")
        );
        assert!(
            !imported
                .extraction
                .as_ref()
                .unwrap()
                .adobe_rendering_equivalent
        );
        assert_eq!(
            t.catalog
                .edit_variant(&master_key)?
                .recipe
                .validate()?
                .settings()
                .exposure_ev,
            1.0
        );
        assert_eq!(
            t.catalog
                .edit_variant(&copied.image)?
                .recipe
                .validate()?
                .settings()
                .exposure_ev,
            -1.0
        );
        // Later user adjustments are authoritative; exact replay returns receipt.
        let edited = crate::edit::Recipe::V1(crate::edit::RecipeV1 {
            exposure_ev: 2.0,
            ..Default::default()
        });
        t.catalog
            .save_edit_recipe(&master_key, imported.edit_revision.unwrap(), &edited)?;
        t.catalog.project_migration_current_develop(None, &master)?;
        assert_eq!(t.catalog.edit_variant(&master_key)?.recipe, edited);
        let mut wrong = develop(&t, 20, 31);
        wrong.expected_edit_revision = t.catalog.edit_variant(&master_key)?.revision;
        assert!(
            t.catalog
                .project_migration_current_develop(Some(&t.source), &wrong)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn compressed_catalog_xmp_retains_original_unknown_fields_and_image_scope() -> Result<()> {
        use crate::catalog_migration::metadata::CatalogXmp;
        let mut t = Test::new()?;
        t.original()?;
        t.project(20)?;
        t.project(21)?;
        let packet = t
            .catalog
            .retained_migration_records(
                t.source.binding_blake3(),
                &t.rows[&40].source.capture_revision,
                Collection::Packets,
                0,
                100,
            )?
            .into_iter()
            .find(|(_, r)| r.fields["source_id"].text().ok() == Some("source-40"))
            .unwrap();
        let raw = retention::field_bytes(&t.catalog.db, packet.0, &packet.1, "raw", LIMIT)?;
        let request = CatalogXmp {
            origin: t.rows[&40].clone(),
            retained_table: t.tables["Adobe_AdditionalMetadata"],
            packet_record: packet.0,
            image: t.link(40, "image", 20),
            import_source: "lightroom".into(),
        };
        let imported = t
            .catalog
            .project_migration_catalog_xmp(Some(&t.source), &request)?;
        let kept = t
            .catalog
            .metadata_packets_for_image(&imported.image, imported.observation.unwrap())?;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].bytes, raw);
        let hist = t
            .catalog
            .metadata_history_for_image(&imported.image, 0, 100)?;
        let model = hist[0].models[0].id;
        assert!(
            String::from_utf8(t.catalog.metadata_model_for_image(&imported.image, model)?)?
                .contains("preserve exactly")
        );
        let copy = mapped_image(&t.catalog.db, "lightroom", &t.rows[&21].source)?;
        assert!(
            t.catalog
                .metadata_history_for_image(&copy, 0, 100)?
                .is_empty()
        );
        let revision = t.catalog.metadata_for_image(&imported.image)?.revision;
        t.catalog.project_migration_catalog_xmp(None, &request)?;
        assert_eq!(
            t.catalog.metadata_for_image(&imported.image)?.revision,
            revision
        );
        Ok(())
    }
    #[test]
    fn oversized_selected_image_settings_and_xmp_have_retained_only_outcomes() -> Result<()> {
        let mut t = Test::with_oversized(true)?;
        t.original()?;
        t.project(20)?;
        let mut image = t.request(22);
        image.decision = Decision::Retain {
            reason: "Oversized source image row retained for review".into(),
        };
        assert!(matches!(
            t.catalog.project_migration_image(None, &image)?.outcome,
            Outcome::Retained { .. }
        ));
        let current = develop(&t, 20, 30);
        let retained = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &current)?;
        assert_eq!(retained.state, "retained_only");
        assert!(retained.edit_revision.is_none());
        assert_eq!(t.catalog.edit_variant(&retained.image)?.revision, 0);
        let packet = t
            .catalog
            .retained_migration_records(
                t.source.binding_blake3(),
                &t.rows[&40].source.capture_revision,
                Collection::Packets,
                0,
                100,
            )?
            .into_iter()
            .find(|(_, r)| r.fields["source_id"].text().ok() == Some("source-40"))
            .unwrap();
        let request = crate::catalog_migration::metadata::CatalogXmp {
            origin: t.rows[&40].clone(),
            retained_table: t.tables["Adobe_AdditionalMetadata"],
            packet_record: packet.0,
            image: t.link(40, "image", 20),
            import_source: "lightroom".into(),
        };
        let retained = t
            .catalog
            .project_migration_catalog_xmp(Some(&t.source), &request)?;
        assert_eq!(retained.state, "retained_only");
        assert!(retained.observation.is_none());
        assert_eq!(t.catalog.metadata_for_image(&retained.image)?.revision, 0);
        assert_eq!(
            t.catalog
                .project_migration_catalog_xmp(None, &request)?
                .input_digest,
            retained.input_digest
        );
        assert_eq!(
            t.catalog
                .project_migration_current_develop(None, &current)?
                .edit_revision,
            None
        );
        Ok(())
    }
    #[test]
    fn oversized_packet_alone_is_retained_without_blocking_other_native_settings() -> Result<()> {
        let mut t = Test::with_oversized_parts(false, true)?;
        t.original()?;
        t.project(20)?;
        let packet = t
            .catalog
            .retained_migration_records(
                t.source.binding_blake3(),
                &t.rows[&40].source.capture_revision,
                Collection::Packets,
                0,
                100,
            )?
            .into_iter()
            .find(|(_, r)| r.fields["source_id"].text().ok() == Some("source-40"))
            .unwrap();
        let request = crate::catalog_migration::metadata::CatalogXmp {
            origin: t.rows[&40].clone(),
            retained_table: t.tables["Adobe_AdditionalMetadata"],
            packet_record: packet.0,
            image: t.link(40, "image", 20),
            import_source: "lightroom".into(),
        };
        let result = t
            .catalog
            .project_migration_catalog_xmp(Some(&t.source), &request)?;
        assert_eq!(result.state, "retained_only");
        assert!(result.observation.is_none());
        let current = develop(&t, 20, 30);
        let applied = t
            .catalog
            .project_migration_current_develop(Some(&t.source), &current)?;
        assert_eq!(
            applied.extraction.unwrap().contribution.exposure_ev,
            Some(1.0)
        );
        Ok(())
    }
}
