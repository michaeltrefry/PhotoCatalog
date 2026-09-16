//! Real retained Lightroom fixture shared by adapter acceptance tests.
use crate::{
    Catalog,
    catalog_edits::VariantKey,
    catalog_migration::{
        images::{Decision, Outcome, Projection, Role},
        organization::{Link, SourceRecord},
        originals::{OriginalDecision, OriginalRequest, SourceKey},
    },
    lightroom::{
        migration_source::{Collection, EvidenceRecord, tests::Fixture},
        plan::Cell,
    },
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use rusqlite::params;
use std::collections::BTreeMap;
pub(super) struct ImportedFixture {
    _temp: tempfile::TempDir,
    pub catalog: Catalog,
    pub keys: Vec<VariantKey>,
}
impl ImportedFixture {
    pub fn new(duplicate: bool, huge: bool) -> Result<Self> {
        Self::with_case(duplicate, huge, "")
    }
    fn with_case(duplicate: bool, huge: bool, case: &str) -> Result<Self> {
        let mut f = Fixture::new();
        let revision = f.revision().to_owned();
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
                // Real catalogs also contain ancillary entities with non-text local
                // keys. They must not poison independent image/history navigation.
                db.execute("INSERT INTO entities VALUES(?1,'ancillary','ImageChangeCounter',NULL,NULL,'{}')", [&revision]).unwrap();
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
        drop(source);
        drop(f);
        Ok(Self {
            _temp: temp,
            catalog,
            keys,
        })
    }
}
