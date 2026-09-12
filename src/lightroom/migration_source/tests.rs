use super::*;
use crate::lightroom::{capture, plan::Plan, source::Source};
use rusqlite::{Connection, params};
use std::{fs, path::Path};

pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    pub(crate) path: std::path::PathBuf,
    pub(crate) seal: InputSeal,
}
fn manifest(path: &Path, tag: &str) -> capture::Manifest {
    let artifact = capture::Artifact {
        source: NativePath::from_path(&path.join(format!("original-{tag}"))),
        role: "main".into(),
        relative: NativePath::from_path(Path::new("original.lrcat")),
        stored: "raw/original.lrcat".into(),
        revision: FileIdentity {
            object: format!("object-{tag}"),
            bytes: 1,
            modified_ns: None,
            changed: "saved".into(),
        },
        blake3: blake3::hash(tag.as_bytes()).to_hex().to_string(),
    };
    let revision = crate::lightroom::json_digest(&vec![artifact.clone()]).unwrap();
    capture::Manifest {
        protocol: 1,
        request: capture::Request {
            source: artifact.source.clone(),
            output: NativePath::from_path(&path.join(tag)),
            include_auxiliary: true,
            closed_application_evidence: None,
            limits: crate::lightroom::Limits::default(),
        },
        state: "captured".into(),
        raw_byte_retention: "complete".into(),
        sqlite_consistency: "consistent_default_sqlite".into(),
        application_consistency: "unverified".into(),
        cooperative_lock_protocol: "fixture".into(),
        artifacts: vec![artifact],
        companion_inventory: vec![],
        absent_companions: vec![],
        issues: vec![],
        wal: None,
        logical_blake3: None,
        logical_revision: None,
        revision_id: Some(revision),
    }
}
fn seal_file(path: &Path, seal: &mut InputSeal) {
    let mut file = Source::open(path, u64::MAX).unwrap();
    seal.identity = file.before.clone();
    seal.blake3 = file.copy_and_hash(None).unwrap();
    seal.approval.roster_blake3 = seal.roster_blake3().unwrap();
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("plan");
        drop(Plan::create(&root).unwrap());
        let path = root.join("inspection.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        let mut selected = None;
        let mut excluded = vec![];
        for (tag, index) in [("selected", 1), ("excluded", 2)] {
            let manifest = manifest(temp.path(), tag);
            let revision = manifest.revision_id.clone().unwrap();
            let bytes = serde_json::to_vec(&manifest).unwrap();
            db.execute("INSERT INTO captures(revision,lineage,path,manifest,stage,evidence_revision) VALUES(?1,'random-lineage','must never open this path',?2,'inspection_complete_with_reported_gaps',9)",params![revision,std::str::from_utf8(&bytes).unwrap()]).unwrap();
            if tag == "selected" {
                selected = Some(SelectedCapture {
                    revision: revision.clone(),
                    family: "family".into(),
                    family_evidence_digest: "a".repeat(64),
                    manifest_blake3: blake3::hash(&bytes).to_hex().to_string(),
                    evidence_revision: 9,
                });
                db.execute("INSERT INTO family_choices VALUES('family',?,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','fixture TEST choice')",[&revision]).unwrap();
            } else {
                excluded.push(revision.clone());
            }
            db.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,expected,retained,state) VALUES(?,'unknown_plugin','[\"raw\",\"bits\",\"text\"]','[]','{}','unsupported_retained_only',1,1,'complete')",[&revision]).unwrap();
            let cells = vec![
                Cell::Blob(vec![0xff, 0, 1]),
                Cell::RealBits(f64::NAN.to_bits()),
                Cell::Text(vec![0xff, 0, 0xfe]),
            ];
            db.execute("INSERT INTO rows(sequence,revision,source_id,table_name,key_json,cells_json) VALUES(?,?,?,'unknown_plugin','[{\"type\":\"Integer\",\"value\":7}]',?)",params![index,revision,format!("lineage-{tag}:7"),serde_json::to_string(&cells).unwrap()]).unwrap();
            db.execute("INSERT INTO paths(sequence,revision,source_id,original,state,evidence) VALUES(?,?,?,'retained path','missing','{\"missing\":true}')",params![index,revision,format!("file-{tag}")]).unwrap();
            db.execute("INSERT INTO packets(sequence,revision,source_id,origin,raw_digest,raw,decoded,detail) VALUES(?,?,?,'catalog',?,?1,NULL,'{\"retained\":true}')",params![index,revision,format!("lineage-{tag}:7"),"b".repeat(64)]).unwrap();
            // Replace the scalar fixture raw with an actual BLOB.
            db.execute(
                "UPDATE packets SET raw=? WHERE sequence=?",
                params![vec![0u8, 255, 3, 4], index],
            )
            .unwrap();
        }
        drop(db);
        let mut seal = InputSeal {
            protocol: 1,
            database: NativePath::from_path(&path),
            identity: FileIdentity {
                object: String::new(),
                bytes: 0,
                modified_ns: None,
                changed: String::new(),
            },
            blake3: String::new(),
            approval: SelectionApproval {
                document_blake3: "d".repeat(64),
                scope: "selected_migration_test".into(),
                roster_blake3: String::new(),
            },
            selected: vec![selected.unwrap()],
            excluded_revisions: excluded,
            supplements: vec![],
        };
        seal_file(&path, &mut seal);
        Self {
            _temp: temp,
            path,
            seal,
        }
    }
    pub(crate) fn revision(&self) -> &str {
        &self.seal.selected[0].revision
    }
    pub(crate) fn open(&self) -> MigrationSource {
        MigrationSource::open(self.seal.clone(), ReadLimits::default()).unwrap()
    }
    pub(crate) fn edit(&mut self, change: impl FnOnce(&Connection)) {
        let db = Connection::open(&self.path).unwrap();
        change(&db);
        drop(db);
        seal_file(&self.path, &mut self.seal);
    }
    pub(crate) fn with_large_cell(bytes: usize) -> Self {
        let mut fixture = Self::new();
        let revision = fixture.revision().to_owned();
        let cells = serde_json::to_string(&vec![
            Cell::Blob(vec![173; bytes]),
            Cell::RealBits((-0.0_f64).to_bits()),
            Cell::Text(vec![255, 0]),
        ])
        .unwrap();
        fixture.edit(|db| {
            db.execute(
                "UPDATE rows SET cells_json=? WHERE revision=?",
                params![cells, revision],
            )
            .unwrap();
        });
        fixture
    }
}

#[test]
fn selected_typed_unknown_rows_and_raw_chunks_preserve_source_bytes() {
    let fixture = Fixture::new();
    let before = fs::read(&fixture.path).unwrap();
    let source = fixture.open();
    let page = source
        .page(fixture.revision(), Collection::Rows, None, 1)
        .unwrap();
    assert!(page.exhausted);
    assert_eq!(page.records.len(), 1);
    let row = page.records[0]
        .retained_row(
            vec!["raw".into(), "bits".into(), "text".into()],
            "unsupported_retained_only".into(),
        )
        .unwrap();
    assert_eq!(
        row.cells,
        vec![
            Cell::Blob(vec![255, 0, 1]),
            Cell::RealBits(f64::NAN.to_bits()),
            Cell::Text(vec![255, 0, 254])
        ]
    );
    let packets = source
        .page(fixture.revision(), Collection::Packets, None, 2)
        .unwrap();
    let Field::Bytes(reference) = &packets.records[0].fields["raw"] else {
        panic!("raw descriptor")
    };
    assert_eq!(source.read_chunk(reference, 1, 2).unwrap(), vec![255, 3]);
    assert!(source.read_chunk(reference, 5, 1).is_err());
    assert_eq!(
        source.read_chunk(reference, 4, 1).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        source
            .capture_manifest(fixture.revision())
            .unwrap()
            .artifacts
            .len(),
        1
    );
    drop(source);
    assert_eq!(before, fs::read(&fixture.path).unwrap());
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!Path::new(&format!("{}{suffix}", fixture.path.display())).exists());
    }
}

#[test]
fn excluded_wrong_seal_and_cross_collection_cursors_fail() {
    let fixture = Fixture::new();
    let source = fixture.open();
    assert!(
        source
            .page(
                &fixture.seal.excluded_revisions[0],
                Collection::Rows,
                None,
                1
            )
            .is_err()
    );
    assert!(
        source
            .capture_manifest(&fixture.seal.excluded_revisions[0])
            .is_err()
    );
    let cursor = source
        .page(fixture.revision(), Collection::Rows, None, 1)
        .unwrap()
        .next
        .unwrap();
    assert!(
        source
            .page(fixture.revision(), Collection::Paths, Some(&cursor), 1)
            .is_err()
    );
    let mut cursor = cursor;
    cursor.seal = "0".repeat(64);
    assert!(
        source
            .page(fixture.revision(), Collection::Rows, Some(&cursor), 1)
            .is_err()
    );
    assert!(
        source
            .page(fixture.revision(), Collection::Rows, None, 1001)
            .is_err()
    );
}

#[test]
fn large_retained_cell_uses_complete_incremental_descriptor() {
    let fixture = Fixture::with_large_cell(17 * 1024 * 1024);
    let cells = serde_json::to_string(&vec![
        Cell::Blob(vec![173; 17 * 1024 * 1024]),
        Cell::RealBits((-0.0_f64).to_bits()),
        Cell::Text(vec![255, 0]),
    ])
    .unwrap();
    let source = fixture.open();
    let page = source
        .page(fixture.revision(), Collection::Rows, None, 1)
        .unwrap();
    let Field::Bytes(reference) = &page.records[0].fields["cells_json"] else {
        panic!("large descriptor")
    };
    assert_eq!(reference.bytes, cells.len() as u64);
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    while offset < reference.bytes {
        let chunk = source.read_chunk(reference, offset, 65536).unwrap();
        offset += chunk.len() as u64;
        hash.update(&chunk);
    }
    assert_eq!(hash.finalize(), blake3::hash(cells.as_bytes()));
    assert!(
        page.records[0]
            .retained_row(vec!["large".into()], "unknown".into())
            .is_err()
    );
}

#[test]
fn stable_mapping_uses_capture_table_and_key_not_random_lineage() {
    let mut fixture = Fixture::new();
    let first = fixture
        .open()
        .stable_source(fixture.revision(), "lineage-selected:7")
        .unwrap();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db| {
        db.execute(
            "UPDATE rows SET source_id='new-lineage:7' WHERE revision=?",
            [revision],
        )
        .unwrap();
    });
    let second = fixture
        .open()
        .stable_source(fixture.revision(), "new-lineage:7")
        .unwrap();
    assert_eq!(first.capture_revision, second.capture_revision);
    assert_eq!(first.table, second.table);
    assert_eq!(first.source_key_blake3, second.source_key_blake3);
    assert_ne!(first.inspection_source_id, second.inspection_source_id);
}

#[test]
fn schema_partition_family_authorization_and_pending_inputs_are_checked() {
    let fixture = Fixture::new();
    for change in 0..5 {
        let mut seal = fixture.seal.clone();
        match change {
            0 => seal.excluded_revisions.clear(),
            1 => seal.approval.scope = "unapproved".into(),
            2 => seal.selected[0].evidence_revision += 1,
            3 => seal.selected[0].family_evidence_digest = "e".repeat(64),
            _ => seal.blake3 = "0".repeat(64),
        };
        if change != 1 {
            seal.approval.roster_blake3 = seal.roster_blake3().unwrap();
        }
        assert!(MigrationSource::open(seal, ReadLimits::default()).is_err());
    }
    for sql in [
        "PRAGMA user_version=2",
        "UPDATE paths SET state='pending'",
        "UPDATE captures SET stage='incomplete_rows'",
    ] {
        let mut fixture = Fixture::new();
        fixture.edit(|db| db.execute_batch(sql).unwrap());
        assert!(MigrationSource::open(fixture.seal, ReadLimits::default()).is_err());
    }
}

#[test]
fn companions_and_changed_file_invalidate_reader_without_recovery() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let fixture = Fixture::new();
        let path = std::path::PathBuf::from(format!("{}{suffix}", fixture.path.display()));
        fs::write(&path, []).unwrap();
        let before = fs::read(&fixture.path).unwrap();
        assert!(MigrationSource::open(fixture.seal.clone(), ReadLimits::default()).is_err());
        assert_eq!(before, fs::read(&fixture.path).unwrap());
        assert_eq!(fs::read(path).unwrap(), Vec::<u8>::new());
    }
    let fixture = Fixture::new();
    let source = fixture.open();
    fs::write(format!("{}-wal", fixture.path.display()), []).unwrap();
    assert!(source.count(fixture.revision(), Collection::Rows).is_err());
    fs::remove_file(format!("{}-wal", fixture.path.display())).unwrap();
    assert!(
        source.count(fixture.revision(), Collection::Rows).is_err(),
        "poison remains after unsafe source resumes"
    );
}

#[test]
fn exact_numeric_key_resolution_reports_ambiguity() {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db|{
        db.execute("INSERT INTO entities VALUES(?,'image','Adobe_images',NULL,NULL,'{}')",[&revision]).unwrap();
        db.execute("INSERT INTO entities VALUES(?,'develop','Adobe_imageDevelopSettings','{\"type\":\"Integer\",\"value\":17913}',NULL,'{}')",[&revision]).unwrap();
        db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?,'image','developSettingsIDCache','Adobe_imageDevelopSettings','{\"type\":\"Integer\",\"value\":17913}')",[&revision]).unwrap();
    });
    let links = fixture
        .open()
        .image_links(fixture.revision(), "image")
        .unwrap();
    assert_eq!(links.current_develop, Resolution::Unique("develop".into()));
    assert_eq!(links.master, Resolution::Missing);
    fixture.edit(|db|{db.execute("INSERT INTO entities VALUES(?,'duplicate','Adobe_imageDevelopSettings','{\"type\":\"Integer\",\"value\":17913}',NULL,'{}')",[&revision]).unwrap();});
    assert_eq!(
        fixture
            .open()
            .image_links(fixture.revision(), "image")
            .unwrap()
            .current_develop,
        Resolution::Ambiguous
    );
}

#[test]
fn read_limits_and_null_decoded_are_not_silent_empty_payloads() {
    let fixture = Fixture::new();
    let source = fixture.open();
    let mut reference = match &source
        .page(fixture.revision(), Collection::Packets, None, 1)
        .unwrap()
        .records[0]
        .fields["raw"]
    {
        Field::Bytes(r) => r.clone(),
        _ => panic!(),
    };
    reference.field = "decoded".into();
    assert!(source.read_chunk(&reference, 0, 1).is_err());
    reference.field = "raw;DELETE FROM packets".into();
    assert!(source.read_chunk(&reference, 0, 1).is_err());
    let limits = ReadLimits {
        vm_steps: 0,
        ..ReadLimits::default()
    };
    assert!(MigrationSource::open(fixture.seal, limits).is_err());
}

#[test]
fn vm_budget_interrupts_work_and_next_small_query_still_works() {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db| {
        db.execute("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<20000) INSERT INTO entities SELECT ?1,printf('source-%08d',x),'unknown',NULL,NULL,'{}' FROM n",[revision]).unwrap();
    });
    let source = MigrationSource::open(
        fixture.seal.clone(),
        ReadLimits {
            vm_steps: 10_000,
            ..ReadLimits::default()
        },
    )
    .unwrap();
    assert!(
        source
            .count(fixture.revision(), Collection::Entities)
            .is_err()
    );
    assert_eq!(
        source
            .page(fixture.revision(), Collection::Entities, None, 1)
            .unwrap()
            .records
            .len(),
        1
    );
}

#[test]
fn supplement_is_source_bound_and_never_rewrites_historical_status() {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    let original = crate::xmp_packets::SourceRevision {
        length: 7,
        blake3: "c".repeat(64),
        modified_unix_ns: Some(42),
    };
    let evidence = serde_json::json!({"inspections":[{"origin":"embedded","revision":original,"status":"Malformed","issues":["invalid PSD resource signature"]}],"packet_gaps":true});
    fixture.edit(|db| {
        db.execute(
            "UPDATE paths SET state='available_packet_gaps',evidence=? WHERE revision=?",
            params![evidence.to_string(), revision],
        )
        .unwrap();
    });
    let pin = SupplementPin {
        revision: revision.clone(),
        source_id: "file-selected".into(),
        origin: "embedded".into(),
        source_revision: original,
        historical_status: crate::xmp_packets::Status::Malformed,
        proof_blake3: "e".repeat(64),
    };
    fixture.seal.supplements.push(pin);
    let before = fs::read(&fixture.path).unwrap();
    let source = fixture.open();
    let paths = source.page(&revision, Collection::Paths, None, 1).unwrap();
    assert_eq!(
        paths.records[0].fields["state"].text().unwrap(),
        "available_packet_gaps"
    );
    drop(source);
    assert_eq!(before, fs::read(&fixture.path).unwrap());
    fixture.seal.supplements[0].source_revision.length += 1;
    assert!(MigrationSource::open(fixture.seal, ReadLimits::default()).is_err());
}

#[test]
fn changed_main_is_rejected_or_windows_write_is_denied() {
    use std::io::{Seek, SeekFrom, Write};
    let fixture = Fixture::new();
    let source = fixture.open();
    match fs::OpenOptions::new().write(true).open(&fixture.path) {
        Ok(mut file) => {
            file.seek(SeekFrom::Start(fixture.seal.identity.bytes - 1))
                .unwrap();
            file.write_all(&[19]).unwrap();
            file.sync_all().unwrap();
            assert!(
                source
                    .page(fixture.revision(), Collection::Rows, None, 1)
                    .is_err()
            );
        }
        Err(error) => {
            #[cfg(not(windows))]
            panic!("unexpected writer refusal: {error}");
            #[cfg(windows)]
            {
                assert_eq!(error.raw_os_error(), Some(32));
                assert_eq!(
                    source.count(fixture.revision(), Collection::Rows).unwrap(),
                    1
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn symlinked_seal_is_rejected_without_following_it() {
    let fixture = Fixture::new();
    let alias = fixture.path.with_file_name("alias.sqlite3");
    std::os::unix::fs::symlink(&fixture.path, &alias).unwrap();
    let mut seal = fixture.seal;
    seal.database = NativePath::from_path(&alias);
    assert!(MigrationSource::open(seal, ReadLimits::default()).is_err());
}

#[test]
fn every_collection_and_tuple_cursor_preserves_selected_scope() {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db|{
        for key in ["a","b","c"] {
            db.execute("INSERT INTO references_out(revision,source_id,field,target_table,target_key) VALUES(?1,'owner',?2,'unknown','same-key')",params![revision,key]).unwrap();
            db.execute("INSERT INTO metadata_facts VALUES(?1,'owner',NULL,'catalog','digest',?2,'42')",params![revision,key]).unwrap();
        }
    });
    let source = fixture.open();
    for collection in [
        Collection::Captures,
        Collection::Rows,
        Collection::Entities,
        Collection::References,
        Collection::Paths,
        Collection::Packets,
        Collection::MetadataFacts,
        Collection::Issues,
        Collection::Tables,
        Collection::SchemaObjects,
    ] {
        let mut cursor = None;
        let mut count = 0;
        loop {
            let page = source
                .page(&revision, collection, cursor.as_ref(), 1)
                .unwrap();
            count += page.records.len();
            cursor = page.next;
            if page.exhausted {
                break;
            }
        }
        assert_eq!(count as u64, source.count(&revision, collection).unwrap());
    }
}

#[test]
fn writer_process_fixture() {
    use std::io::{Read, Write};
    let Some(path) = std::env::var_os("PHOTOCATALOG_MIGRATION_WRITER_FIXTURE") else {
        return;
    };
    let db = Connection::open(path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    println!("MIGRATION_WRITER_READY");
    std::io::stdout().flush().unwrap();
    let mut release = [0];
    std::io::stdin().read_exact(&mut release).unwrap();
    db.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn active_cooperative_writer_is_rejected_before_reading_data() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
    };
    let fixture = Fixture::new();
    let before = fs::read(&fixture.path).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lightroom::migration_source::tests::writer_process_fixture",
            "--nocapture",
        ])
        .env("PHOTOCATALOG_MIGRATION_WRITER_FIXTURE", &fixture.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ready = false;
    for _ in 0..8 {
        let mut line = String::new();
        if output.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.contains("MIGRATION_WRITER_READY") {
            ready = true;
            break;
        }
    }
    let rejected =
        ready && MigrationSource::open(fixture.seal.clone(), ReadLimits::default()).is_err();
    // Release and reap before assertions so a regression cannot strand the writer.
    let _ = child.stdin.take().unwrap().write_all(&[1]);
    let status = child.wait().unwrap();
    assert!(ready && status.success() && rejected);
    assert_eq!(before, fs::read(&fixture.path).unwrap());
}
