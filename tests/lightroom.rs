use photocatalog::{
    lightroom::{
        Limits,
        capture::{self, Request},
        discovery,
        plan::{Cell, Plan, decode_catalog_xmp},
    },
    storage_volume::NativePath,
};
use rusqlite::{Connection, params};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

fn fixture(path: &Path, version: &str, touch: f64) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE Adobe_variablesTable(id_local INTEGER PRIMARY KEY,name TEXT,value TEXT);
CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,absolutePath TEXT);
CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFolder INTEGER,pathFromRoot TEXT,parentId INTEGER);
CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY,id_global TEXT,folder INTEGER,baseName TEXT,extension TEXT,idx_filename TEXT);
CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY,id_global TEXT,rootFile INTEGER,masterImage INTEGER,copyName TEXT,rating INTEGER,pick INTEGER,colorLabels TEXT,touchTime REAL);
CREATE TABLE Adobe_imageDevelopSettings(id_local INTEGER PRIMARY KEY,image INTEGER,hasBigData INTEGER,text TEXT);
CREATE TABLE Adobe_libraryImageDevelopHistoryStep(id_local INTEGER PRIMARY KEY,id_global TEXT,image INTEGER,dateCreated REAL,text TEXT);
CREATE TABLE Adobe_libraryImageDevelopSnapshot(id_local INTEGER PRIMARY KEY,id_global TEXT,image INTEGER,text TEXT);
CREATE TABLE Adobe_AdditionalMetadata(id_local INTEGER PRIMARY KEY,image INTEGER,xmp BLOB);
CREATE TABLE AgLibraryKeyword(id_local INTEGER PRIMARY KEY,id_global TEXT,name TEXT,parent INTEGER);
CREATE TABLE AgLibraryKeywordImage(id_local INTEGER PRIMARY KEY,image INTEGER,tag INTEGER);
CREATE TABLE AgLibraryCollection(id_local INTEGER PRIMARY KEY,name TEXT,parent INTEGER);
CREATE TABLE AgLibraryCollectionImage(id_local INTEGER PRIMARY KEY,image INTEGER,collection INTEGER,positionInCollection TEXT);
CREATE TABLE Opaque(k INTEGER PRIMARY KEY,n,b,t,r);
CREATE VIEW Unexecuted AS SELECT load_extension('never-run');
INSERT INTO Adobe_variablesTable VALUES(1,'Adobe_storeProviderID','synthetic-provider');
INSERT INTO AgLibraryRootFolder VALUES(1,'root-global','/definitely-missing-synthetic-root/');
INSERT INTO AgLibraryFolder VALUES(1,'folder-global',1,'2022/',NULL);
INSERT INTO AgLibraryFile VALUES(1,'file-global',1,'source','CR2','source.CR2');
INSERT INTO Adobe_imageDevelopSettings VALUES(1,1,1,'return os.execute(\"never execute me\")');
INSERT INTO Adobe_libraryImageDevelopHistoryStep VALUES(1,'history-global',1,10,'opaque nested Adobe text');
INSERT INTO Adobe_libraryImageDevelopSnapshot VALUES(1,'snapshot-global',2,'opaque snapshot');
INSERT INTO AgLibraryKeyword VALUES(1,'keyword-a','alpha',2),(2,'keyword-b','beta',1);
INSERT INTO AgLibraryKeywordImage VALUES(1,1,1),(2,999,2);
INSERT INTO AgLibraryCollection VALUES(1,'collection',NULL);
INSERT INTO AgLibraryCollectionImage VALUES(1,2,1,'opaque-order');
INSERT INTO Opaque VALUES(1,NULL,x'00ff01',CAST(x'fffe00' AS TEXT),1.25);") .unwrap();
    db.execute(
        "INSERT INTO Adobe_variablesTable VALUES(2,'Adobe_DBVersion',?)",
        [version],
    )
    .unwrap();
    db.execute("INSERT INTO Adobe_images VALUES(1,'master-global',1,NULL,NULL,5,1,'red',?),(2,'copy-global',1,1,'independent copy',2,0,'',?)",params![touch,touch]).unwrap();
    let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><unknown/></x:xmpmeta>";
    let mut zipped = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
    zipped.write_all(xmp).unwrap();
    let mut packet = (xmp.len() as u32).to_be_bytes().to_vec();
    packet.extend(zipped.finish().unwrap());
    db.execute(
        "INSERT INTO Adobe_AdditionalMetadata VALUES(1,1,?)",
        [packet],
    )
    .unwrap();
    db
}
fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lightroom_inspect"))
}
fn request(source: &Path, output: &Path) -> Request {
    Request {
        source: NativePath::from_path(source),
        output: NativePath::from_path(output),
        include_auxiliary: true,
        closed_application_evidence: None,
        limits: Limits::default(),
    }
}
fn capture_file(source: &Path, output: &Path) -> capture::Manifest {
    capture::spawn(&worker(), &request(source, output)).unwrap()
}
fn finish(plan: &mut Plan, revision: &str) {
    for _ in 0..1000 {
        let result = plan.resume(revision, 7).unwrap();
        if result.stage != "pending" {
            return;
        }
    }
    panic!("inspection did not finish");
}
fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = vec![];
    let mut todo = vec![root.to_owned()];
    while let Some(path) = todo.pop() {
        for item in fs::read_dir(path).unwrap() {
            let item = item.unwrap();
            if item.file_type().unwrap().is_dir() {
                todo.push(item.path());
            } else {
                out.push((
                    item.path().strip_prefix(root).unwrap().into(),
                    fs::read(item.path()).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
#[ignore = "fixture writer process only"]
fn sqlite_writer_worker() {
    use std::io::Read;
    let path = std::env::var_os("PHOTOCATALOG_LR_WRITER_PATH").unwrap();
    let mode = std::env::var("PHOTOCATALOG_LR_WRITER_MODE").unwrap();
    let ready = std::env::var_os("PHOTOCATALOG_LR_WRITER_READY").unwrap();
    let db = Connection::open(Path::new(&path)).unwrap();
    db.pragma_update(None, "journal_mode", mode).unwrap();
    db.execute_batch("BEGIN IMMEDIATE; UPDATE Adobe_images SET rating=1;")
        .unwrap();
    fs::write(ready, b"ready").unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    db.execute_batch("ROLLBACK").unwrap();
}
#[test]
fn retains_typed_rows_virtual_copies_opaque_instructions_and_resumes_after_restart() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("originals");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("2022-v13.lrcat");
    drop(fixture(&source, "1300000", 100.0));
    fs::create_dir(originals.join("2022-v13.lrcat-data")).unwrap();
    fs::write(
        originals.join("2022-v13.lrcat-data/mask.acr"),
        b"opaque companion",
    )
    .unwrap();
    let before = tree(&originals);
    let captured = temp.path().join("captured");
    let manifest = capture_file(&source, &captured);
    assert_eq!(manifest.state, "captured");
    assert_eq!(manifest.raw_byte_retention, "complete");
    assert_eq!(manifest.application_consistency, "unverified");
    let path = temp.path().join("plan");
    let mut plan = Plan::create(&path).unwrap();
    let revision = plan.add_capture(&captured).unwrap();
    assert_eq!(plan.add_capture(&captured).unwrap(), revision);
    plan.resume(&revision, 3).unwrap();
    drop(plan);
    let mut plan = Plan::open(&path).unwrap();
    finish(&mut plan, &revision);
    let rows = plan.rows(&revision, Some("Adobe_images"), 0, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].source_id, rows[1].source_id);
    let raw = plan.rows(&revision, Some("Opaque"), 0, 10).unwrap();
    assert!(raw[0].cells.contains(&Cell::Blob(vec![0, 255, 1])));
    assert!(raw[0].cells.contains(&Cell::Text(vec![255, 254, 0])));
    assert!(raw[0].cells.contains(&Cell::RealBits(1.25f64.to_bits())));
    let report = plan.report(&revision).unwrap();
    assert!(report.tables.iter().all(|t| t.state == "complete"));
    assert!(report.issues.iter().any(|i| i.code == "hierarchy_cycle"));
    assert!(report.issues.iter().any(|i| i.code == "dangling_reference"));
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.code == "required_auxiliary_missing")
    );
    assert_eq!(plan.check_paths(&revision, 10, false).unwrap(), 1);
    assert_eq!(plan.paths(&revision, 0, 10).unwrap()[0]["state"], "missing");
    assert_eq!(before, tree(&originals));
}
#[test]
fn active_rollback_and_wal_writers_are_rejected_without_changing_sources() {
    for mode in ["DELETE", "WAL"] {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let originals = temp.path().join("source");
        fs::create_dir(&originals).unwrap();
        let source = originals.join("test.lrcat");
        drop(fixture(&source, "1300000", 10.0));
        let ready = temp.path().join("ready");
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "sqlite_writer_worker",
                    "--nocapture",
                ])
                .env("PHOTOCATALOG_LR_WRITER_PATH", &source)
                .env("PHOTOCATALOG_LR_WRITER_MODE", mode)
                .env("PHOTOCATALOG_LR_WRITER_READY", &ready)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline);
            assert!(child.0.try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let before = tree(&originals);
        let manifest = capture_file(&source, &temp.path().join("capture"));
        assert_eq!(manifest.state, "failed", "{mode}: {manifest:?}");
        assert!(
            manifest.issues.iter().any(|i| i.detail.contains("lock")),
            "{manifest:?}"
        );
        assert_eq!(before, tree(&originals));
        child.0.stdin.take().unwrap().write_all(&[1]).unwrap();
        assert!(child.0.wait().unwrap().success());
    }
}
#[test]
fn captures_committed_wal_and_preserves_original_sidecars() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    let db = fixture(&source, "1300000", 10.0);
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    db.execute("UPDATE Adobe_images SET rating=4", []).unwrap();
    let before = tree(&originals);
    let destination = temp.path().join("capture");
    let manifest = capture_file(&source, &destination);
    assert_eq!(manifest.state, "captured", "{manifest:?}");
    assert!(manifest.wal.as_ref().unwrap().last_commit_frame > 0);
    assert_eq!(before, tree(&originals));
    let logical = Connection::open_with_flags(
        destination.join("logical.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        logical
            .query_row("SELECT min(rating) FROM Adobe_images", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
}
#[test]
fn changed_evidence_and_corrupted_raw_artifacts_are_rejected() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    drop(fixture(&source, "1300000", 10.0));
    let destination = temp.path().join("capture");
    let manifest = capture_file(&source, &destination);
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&destination).unwrap();
    plan.resume(&revision, 2).unwrap();
    let db = Connection::open(destination.join("logical.sqlite3")).unwrap();
    db.execute("UPDATE Adobe_images SET rating=0", []).unwrap();
    drop(db);
    assert!(
        plan.resume(&revision, 100)
            .unwrap_err()
            .to_string()
            .contains("revision changed")
    );
    fs::write(destination.join(&manifest.artifacts[0].stored), b"corrupt").unwrap();
    let mut other = Plan::create(&temp.path().join("other")).unwrap();
    assert!(other.add_capture(&destination).is_err());
}
#[test]
fn discovery_limits_and_suffixes_are_hints_not_selection() {
    assert_eq!(
        discovery::filename_hint("2014-v10-v11-v13-3"),
        ("2014".into(), Some(13))
    );
    assert_eq!(
        discovery::filename_hint("named-vault"),
        ("named-vault".into(), None)
    );
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    for i in 0..20 {
        fs::create_dir(temp.path().join(format!("dir-{i}"))).unwrap();
    }
    let inventory = discovery::discover(
        temp.path(),
        &Limits {
            max_files: 5,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!inventory.complete);
    assert_eq!(inventory.entries, 6);
}
#[test]
fn copied_mtime_renamed_families_and_newer_2018_suffix_require_explicit_evidence_choice() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    for (index, name, touch) in [
        (0, "2018-v13.lrcat", 10.0),
        (1, "2018-v13-3.lrcat", 20.0),
        (2, "Renamed-family-v13.lrcat", 15.0),
    ] {
        let source = originals.join(name);
        drop(fixture(&source, "1300000", touch));
        let output = temp.path().join(format!("capture-{index}"));
        capture_file(&source, &output);
        let revision = plan.add_capture(&output).unwrap();
        finish(&mut plan, &revision);
    }
    plan.register_inventory(&discovery::discover(&originals, &Limits::default()).unwrap())
        .unwrap();
    let report = plan.families().unwrap();
    assert_eq!(report.families.len(), 1);
    let family = &report.families[0];
    assert!(family.selected.is_none());
    let suggested = family.suggested.clone().unwrap();
    assert!(
        family
            .members
            .iter()
            .find(|m| m.revision_id == suggested)
            .unwrap()
            .source
            .to_path()
            .unwrap()
            .ends_with("2018-v13-3.lrcat")
    );
    plan.choose(
        &family.id,
        &suggested,
        &family.evidence_digest,
        "Reviewed internal touch progression and copied dates",
    )
    .unwrap();
    assert_eq!(plan.families().unwrap().families[0].excluded.len(), 2);
    let source = originals.join("2018-v13-4.lrcat");
    drop(fixture(&source, "1300000", 25.0));
    plan.register_inventory(&discovery::discover(&originals, &Limits::default()).unwrap())
        .unwrap();
    assert!(plan.families().unwrap().families[0].selected.is_none());
}
#[test]
fn xmp_expansion_is_exact_bounded_and_rejects_trailing_bytes() {
    let mut zipped = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
    zipped.write_all(b"<x/>").unwrap();
    let mut bytes = 4u32.to_be_bytes().to_vec();
    bytes.extend(zipped.finish().unwrap());
    assert_eq!(decode_catalog_xmp(&bytes, 10).unwrap(), b"<x/>");
    assert!(decode_catalog_xmp(&bytes, 3).is_err());
    let mut wrong = bytes.clone();
    wrong[3] = 5;
    assert!(decode_catalog_xmp(&wrong, 10).is_err());
    bytes.push(0);
    assert!(decode_catalog_xmp(&bytes, 10).is_err());
}
#[test]
fn overlap_and_special_sources_do_not_create_capture_artifacts() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let source = temp.path().join("test.lrcat");
    drop(fixture(&source, "1300000", 10.0));
    let before = tree(temp.path());
    assert!(capture::spawn(&worker(), &request(&source, &temp.path().join("nested"))).is_err());
    assert_eq!(before, tree(temp.path()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = temp.path().join("link.lrcat");
        symlink(&source, &link).unwrap();
        assert!(
            capture::spawn(
                &worker(),
                &request(&link, &temp.path().with_extension("capture"))
            )
            .is_err()
        );
    }
}
#[test]
fn main_only_and_required_auxiliary_gaps_are_distinct_from_sqlite_consistency() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    drop(fixture(&source, "1300000", 10.0));
    fs::create_dir(originals.join("test.lrcat-data")).unwrap();
    fs::write(originals.join("test.lrcat-data/opaque"), b"data").unwrap();
    let output = temp.path().join("capture");
    let mut request = request(&source, &output);
    request.include_auxiliary = false;
    let manifest = capture::spawn(&worker(), &request).unwrap();
    assert_eq!(manifest.state, "captured");
    assert_eq!(
        manifest.raw_byte_retention,
        "auxiliary_omitted_for_discovery"
    );
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&output).unwrap();
    finish(&mut plan, &revision);
    assert!(
        plan.report(&revision)
            .unwrap()
            .issues
            .iter()
            .any(|i| i.code == "required_auxiliary_missing")
    );
}

#[test]
fn unrelated_v1_database_is_rejected_before_any_journal_or_schema_mutation() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let path = temp.path().join("inspection.sqlite3");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=1;PRAGMA journal_mode=DELETE;CREATE TABLE unrelated(v);INSERT INTO unrelated VALUES('owned evidence');").unwrap();
    drop(db);
    let before = tree(temp.path());
    assert!(Plan::open(temp.path()).is_err());
    assert_eq!(tree(temp.path()), before);
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        db.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
}
#[test]
fn sidecar_survives_missing_original_and_conflicts_with_retained_catalog_facts() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("catalogs");
    fs::create_dir(&originals).unwrap();
    let photo_root = temp.path().join("photos");
    fs::create_dir(&photo_root).unwrap();
    fs::create_dir(photo_root.join("2022")).unwrap();
    let source = originals.join("test.lrcat");
    let db = fixture(&source, "1300000", 10.0);
    db.execute(
        "UPDATE AgLibraryRootFolder SET absolutePath=?",
        [photo_root.to_str().unwrap()],
    )
    .unwrap();
    let packet = |rating| {
        format!(r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="{rating}"/></rdf:RDF></x:xmpmeta>"#).into_bytes()
    };
    db.execute(
        "UPDATE Adobe_AdditionalMetadata SET xmp=CAST(? AS TEXT)",
        [packet(5)],
    )
    .unwrap();
    drop(db);
    let sidecar = photo_root.join("2022/source.xmp");
    let raw = packet(1);
    fs::write(&sidecar, &raw).unwrap();
    let before = tree(&photo_root);
    let captured = temp.path().join("capture");
    capture_file(&source, &captured);
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&captured).unwrap();
    finish(&mut plan, &revision);
    assert_eq!(plan.check_paths(&revision, 10, false).unwrap(), 1);
    plan.register_inventory(&discovery::discover(&originals, &Limits::default()).unwrap())
        .unwrap();
    let before_family = plan.families().unwrap().families.remove(0);
    plan.choose(
        &before_family.id,
        &revision,
        &before_family.evidence_digest,
        "fixture evidence reviewed before packet enrichment",
    )
    .unwrap();
    let before_stage = plan.report(&revision).unwrap().stage;
    assert_eq!(
        plan.families().unwrap().families[0].selected.as_deref(),
        Some(revision.as_str())
    );
    let prior_packets = plan.packets(&revision, 0, 100).unwrap();
    let prior_paths = plan.paths(&revision, 0, 100).unwrap();
    let owned = Connection::open(temp.path().join("plan/inspection.sqlite3")).unwrap();
    owned.execute_batch("CREATE TRIGGER fail_evidence_revision BEFORE UPDATE OF evidence_revision ON captures BEGIN SELECT RAISE(ABORT,'fixture publication failure'); END;").unwrap();
    drop(owned);
    assert!(plan.check_paths(&revision, 10, true).is_err());
    assert_eq!(plan.packets(&revision, 0, 100).unwrap(), prior_packets);
    assert_eq!(plan.paths(&revision, 0, 100).unwrap(), prior_paths);
    assert_eq!(
        plan.families().unwrap().families[0].evidence_digest,
        before_family.evidence_digest
    );
    let owned = Connection::open(temp.path().join("plan/inspection.sqlite3")).unwrap();
    owned
        .execute_batch("DROP TRIGGER fail_evidence_revision")
        .unwrap();
    drop(owned);
    assert_eq!(plan.check_paths(&revision, 10, true).unwrap(), 1);
    let after_family = plan.families().unwrap().families.remove(0);
    assert_eq!(plan.report(&revision).unwrap().stage, before_stage);
    assert_ne!(before_family.evidence_digest, after_family.evidence_digest);
    assert!(after_family.selected.is_none());
    assert!(after_family.issues.iter().any(|i| i.contains("stale")));
    // A resume after completion must not erase path evidence or re-run reconciliation.
    assert_eq!(plan.resume(&revision, 1).unwrap().retained_this_call, 0);
    assert_eq!(
        plan.families().unwrap().families[0].evidence_digest,
        after_family.evidence_digest
    );
    let packets = plan.packets(&revision, 0, 100).unwrap();
    let record = packets
        .iter()
        .find(|v| v["origin"] == "sidecar_xmp:packet:0")
        .unwrap();
    let bytes = plan
        .packet_bytes(
            &revision,
            record["sequence"].as_i64().unwrap(),
            false,
            0,
            1024,
        )
        .unwrap();
    assert_eq!(
        serde_json::from_value::<Cell>(bytes["bytes"].clone()).unwrap(),
        Cell::Blob(raw)
    );
    assert!(
        !plan
            .metadata_conflicts(&revision, 0, 100)
            .unwrap()
            .is_empty()
    );
    assert_eq!(plan.paths(&revision, 0, 10).unwrap()[0]["state"], "missing");
    assert_eq!(tree(&photo_root), before);
}
#[test]
fn oversized_derived_row_is_retained_in_snapshot_and_never_claimed_complete() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    let db = fixture(&source, "1300000", 10.0);
    db.execute(
        "INSERT INTO Opaque(k,b) VALUES(2,zeroblob(?))",
        [5 * 1024 * 1024],
    )
    .unwrap();
    drop(db);
    let captured = temp.path().join("capture");
    let manifest = capture_file(&source, &captured);
    assert_eq!(manifest.state, "captured");
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&captured).unwrap();
    finish(&mut plan, &revision);
    let report = plan.report(&revision).unwrap();
    let table = report.tables.iter().find(|t| t.name == "Opaque").unwrap();
    assert_eq!(table.expected, Some(2));
    assert_eq!(table.state, "failed");
    assert!(table.issue.as_ref().unwrap().contains("byte budget"));
    assert_eq!(report.stage, "incomplete_rows");
    let db = Connection::open_with_flags(
        captured.join("logical.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row("SELECT length(b) FROM Opaque WHERE k=2", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5 * 1024 * 1024
    );
}
#[test]
fn raw_artifact_digest_is_verified_even_for_an_idempotent_add() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    drop(fixture(&source, "1300000", 10.0));
    let output = temp.path().join("capture");
    let manifest = capture_file(&source, &output);
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    plan.add_capture(&output).unwrap();
    fs::write(
        output.join(&manifest.artifacts[0].stored),
        b"corrupt raw only",
    )
    .unwrap();
    assert!(
        plan.add_capture(&output)
            .unwrap_err()
            .to_string()
            .contains("raw artifact")
    );
}
#[test]
fn malformed_and_missing_wal_evidence_never_becomes_a_complete_capture() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("test.lrcat");
    let db = fixture(&source, "1300000", 10.0);
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    db.execute("UPDATE Adobe_images SET rating=3", []).unwrap();
    let captured = temp.path().join("captured");
    let report = capture_file(&source, &captured);
    let artifact = report.artifacts.iter().find(|a| a.role == "wal").unwrap();
    let mut bytes = fs::read(captured.join(&artifact.stored)).unwrap();
    let wal = temp.path().join("bad.wal");
    bytes[24] ^= 1;
    fs::write(&wal, &bytes).unwrap();
    assert!(
        photocatalog::lightroom::wal::validate(&wal)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    bytes[24] ^= 1;
    bytes.pop();
    fs::write(&wal, &bytes).unwrap();
    assert!(
        photocatalog::lightroom::wal::validate(&wal)
            .unwrap()
            .trailing_bytes
            > 0
    );
    drop(db);
    fs::write(originals.join("test.lrcat-wal"), bytes).unwrap();
    let result = capture_file(&source, &temp.path().join("missing-shm"));
    assert_eq!(result.state, "failed");
    assert!(
        result
            .issues
            .iter()
            .any(|i| i.detail.contains("without lockable SHM"))
    );
}
#[cfg(unix)]
#[test]
fn fifo_source_rejection_has_a_subprocess_timeout_and_owned_cleanup() {
    use std::{
        ffi::CString,
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let fifo = temp.path().join("source.lrcat");
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut child = ChildGuard(
        Command::new(worker())
            .arg("capture-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    serde_json::to_writer(
        child.0.stdin.take().unwrap(),
        &request(&fifo, &temp.path().join("result")),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "FIFO capture hung");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    let mut message = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut message)
        .unwrap();
    assert!(message.contains("regular file"));
    assert!(!temp.path().join("result").exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn missing_snapshot_timestamp_cannot_win_by_schema_version() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    for (index, version) in ["1100000", "1300000"].iter().enumerate() {
        let source = originals.join(format!("2022-v{index}.lrcat"));
        let db = fixture(&source, version, 10.0);
        if index == 0 {
            db.execute_batch("ALTER TABLE Adobe_libraryImageDevelopSnapshot ADD COLUMN dateCreated REAL; UPDATE Adobe_libraryImageDevelopSnapshot SET dateCreated=50;").unwrap();
        }
        drop(db);
        let output = temp.path().join(format!("capture-{index}"));
        capture_file(&source, &output);
        let revision = plan.add_capture(&output).unwrap();
        finish(&mut plan, &revision);
    }
    let family = plan.families().unwrap().families.remove(0);
    assert!(family.suggested.is_none());
    assert!(
        family
            .issues
            .iter()
            .any(|i| i.contains("Snapshot recency evidence is incomparable"))
    );
}

#[test]
fn duplicate_image_or_file_ids_never_arbitrarily_associate_catalog_metadata() {
    for table in ["Adobe_images", "AgLibraryFile"] {
        let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let originals = temp.path().join("source");
        fs::create_dir(&originals).unwrap();
        let source = originals.join("2022.lrcat");
        let db = fixture(&source, "1300000", 10.0);
        // Rebuild without uniqueness deliberately; duplicate local IDs are different retained rows.
        db.execute_batch(&format!("ALTER TABLE {table} RENAME TO Old; CREATE TABLE {table} AS SELECT * FROM Old; INSERT INTO {table} SELECT * FROM Old WHERE id_local=1; UPDATE {table} SET id_global='duplicate-global' WHERE rowid=(SELECT max(rowid) FROM {table}); DROP TABLE Old;")).unwrap();
        db.execute("UPDATE Adobe_AdditionalMetadata SET xmp=?",[r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="5"/></rdf:RDF></x:xmpmeta>"#]).unwrap();
        drop(db);
        let before = tree(&originals);
        let output = temp.path().join("capture");
        capture_file(&source, &output);
        let root = temp.path().join("plan");
        let mut plan = Plan::create(&root).unwrap();
        let revision = plan.add_capture(&output).unwrap();
        finish(&mut plan, &revision);
        let db = Connection::open_with_flags(
            root.join("inspection.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (facts,associated):(i64,i64)=db.query_row("SELECT count(*),count(file_source_id) FROM metadata_facts WHERE revision=? AND origin='catalog'",[&revision],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert!(facts > 0, "fixture must project real metadata");
        assert_eq!(associated, 0, "{table}");
        assert!(
            plan.issues(&revision, 0, 100)
                .unwrap()
                .iter()
                .any(|i| i["code"] == "ambiguous_metadata_owner")
        );
        assert!(!plan.packets(&revision, 0, 100).unwrap().is_empty());
        assert_eq!(tree(&originals), before);
    }
}

#[test]
fn same_locator_with_distinct_global_ids_is_reported_without_claiming_identity() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let mut revisions = vec![];
    for (index, name) in ["Family-a", "Family-b"].iter().enumerate() {
        let source = originals.join(format!("{name}.lrcat"));
        let db = fixture(&source, "1300000", 10.0);
        if index == 1 {
            db.execute_batch("UPDATE Adobe_variablesTable SET value='other-provider' WHERE name='Adobe_storeProviderID'; UPDATE Adobe_images SET id_global=id_global||'-other'; UPDATE AgLibraryFile SET id_global=id_global||'-other';").unwrap();
        }
        drop(db);
        let output = temp.path().join(format!("capture-{index}"));
        capture_file(&source, &output);
        let revision = plan.add_capture(&output).unwrap();
        finish(&mut plan, &revision);
        revisions.push(revision);
    }
    let families = plan.families().unwrap().families;
    assert_eq!(families.len(), 2);
    for family in families {
        plan.choose(
            &family.id,
            &family.members[0].revision_id,
            &family.evidence_digest,
            "independent named fixture family",
        )
        .unwrap();
    }
    let report = plan.families().unwrap();
    assert_eq!(report.conflict_count, 0);
    assert_eq!(report.possible_path_collision_count, 1);
    assert_eq!(report.possible_path_collisions.len(), 1);
    let page = plan
        .path_collisions(&revisions[0], &revisions[1], 0, 0, 1)
        .unwrap();
    assert_eq!(
        page[0]["classification"],
        "possible_path_collision_not_identity"
    );
    assert_ne!(page[0]["left_source_id"], page[0]["right_source_id"]);
    assert!(
        plan.path_collisions(
            &revisions[0],
            &revisions[1],
            page[0]["left_sequence"].as_i64().unwrap(),
            page[0]["right_sequence"].as_i64().unwrap(),
            1
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn native_without_rowid_metadata_and_invalid_utf8_text_cursors_resume_losslessly() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("source");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("2022.lrcat");
    let db = fixture(&source, "1300000", 10.0);
    db.execute_batch("CREATE TABLE ByteKeys(k TEXT NOT NULL,n INTEGER NOT NULL,v BLOB,PRIMARY KEY(k,n)) WITHOUT
ROWID; INSERT INTO ByteKeys VALUES(CAST(x'ff' AS TEXT),1,x'00'),(CAST(x'ff' AS TEXT),2,x'01'),(CAST(x'fffe' AS TEXT),1,x'02');").unwrap();
    drop(db);
    let output = temp.path().join("capture");
    capture_file(&source, &output);
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&output).unwrap();
    for _ in 0..1000 {
        if plan.resume(&revision, 1).unwrap().stage != "pending" {
            break;
        }
    }
    let rows = plan.rows(&revision, Some("ByteKeys"), 0, 100).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].source_key,
        vec![Cell::Text(vec![255]), Cell::Integer(1)]
    );
    assert_eq!(
        rows[1].source_key,
        vec![Cell::Text(vec![255]), Cell::Integer(2)]
    );
    assert_eq!(
        rows[2].source_key,
        vec![Cell::Text(vec![255, 254]), Cell::Integer(1)]
    );
    assert_eq!(
        plan.report(&revision)
            .unwrap()
            .tables
            .iter()
            .find(|t| t.name == "ByteKeys")
            .unwrap()
            .state,
        "complete"
    );
}

#[test]
fn emitted_packet_page_includes_newline_in_exact_byte_limit() {
    use photocatalog::lightroom::PAGE_BYTES;
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let root = temp.path().join("plan");
    let plan = Plan::create(&root).unwrap();
    let db = Connection::open(root.join("inspection.sqlite3")).unwrap();
    db.execute_batch("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES('r','id','fixture','digest',x'00','');").unwrap();
    let overhead = serde_json::to_vec(&plan.packets("r", 0, 100).unwrap())
        .unwrap()
        .len();
    let payload = "x".repeat(PAGE_BYTES - 1 - overhead);
    db.execute("UPDATE packets SET detail=? WHERE sequence=1", [&payload])
        .unwrap();
    db.execute_batch("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES('r','id2','fixture','digest2',x'00','next');").unwrap();
    let page = plan.packets("r", 0, 100).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(serde_json::to_vec(&page).unwrap().len() + 1, PAGE_BYTES);
    drop(db);
    drop(plan);
    let output = std::process::Command::new(worker())
        .arg("packets")
        .arg(&root)
        .args(["r", "--limit", "100"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.len(), PAGE_BYTES);
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!(page)
    );
    let plan = Plan::open(&root).unwrap();
    assert_eq!(plan.packets("r", 1, 100).unwrap().len(), 1);
    let db = Connection::open(root.join("inspection.sqlite3")).unwrap();
    db.execute(
        "UPDATE packets SET detail=? WHERE sequence=1",
        [payload + "x"],
    )
    .unwrap();
    assert!(
        plan.packets("r", 0, 100).is_err(),
        "one extra byte cannot hide in framing"
    );
}

#[test]
fn create_cli_reports_inspection_schema_three() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plan");
    let output = std::process::Command::new(worker())
        .arg("create")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], 3);
    let db = Connection::open_with_flags(
        root.join("inspection.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn real_develop_cache_foreign_keys_match_integer_targets_without_retyping_rows() {
    let temp = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let originals = temp.path().join("originals");
    fs::create_dir(&originals).unwrap();
    let source = originals.join("catalog.lrcat");
    let db = fixture(&source, "1000000", 10.0);
    // Actual observed shape: id_local INTEGER PRIMARY KEY, cache column with no
    // declared affinity and integral REAL values. The old typed JSON join fails.
    db.execute_batch(
        "ALTER TABLE Adobe_images ADD COLUMN developSettingsIDCache;
INSERT INTO Adobe_imageDevelopSettings VALUES(2,2,0,'opaque virtual-copy develop settings');
UPDATE Adobe_images SET developSettingsIDCache=CAST(id_local AS REAL);",
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT typeof(developSettingsIDCache) FROM Adobe_images WHERE id_local=1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "real"
    );
    let raw_xmp: Vec<u8> = db
        .query_row("SELECT xmp FROM Adobe_AdditionalMetadata", [], |r| r.get(0))
        .unwrap();
    drop(db);
    let original_bytes = fs::read(&source).unwrap();
    let captured = temp.path().join("capture");
    capture_file(&source, &captured);
    let captured_before = tree(&captured);
    let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
    let revision = plan.add_capture(&captured).unwrap();
    finish(&mut plan, &revision);
    let report = plan.report(&revision).unwrap();
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.code == "unknown_schema_version" && i.detail.contains("1000000"))
    );
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.code == "dangling_reference" && i.detail.contains("developSettingsIDCache"))
    );
    // The fixture's unrelated genuinely absent keyword-image target remains an issue.
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.code == "dangling_reference" && i.detail.contains("999"))
    );
    assert_eq!(report.counts["retained_virtual_copies"], 1);
    let images = plan.rows(&revision, Some("Adobe_images"), 0, 10).unwrap();
    assert_eq!(images.len(), 2);
    for (index, row) in images.iter().enumerate() {
        let field = |name: &str| &row.cells[row.columns.iter().position(|c| c == name).unwrap()];
        assert_eq!(field("id_local"), &Cell::Integer(index as i64 + 1));
        assert_eq!(
            field("developSettingsIDCache"),
            &Cell::RealBits((index as f64 + 1.0).to_bits())
        );
        assert_eq!(
            field("id_global"),
            &Cell::Text(if index == 0 {
                b"master-global".to_vec()
            } else {
                b"copy-global".to_vec()
            })
        );
    }
    assert_ne!(images[0].source_id, images[1].source_id);
    let metadata = plan
        .rows(&revision, Some("Adobe_AdditionalMetadata"), 0, 10)
        .unwrap();
    let xmp = metadata[0].columns.iter().position(|c| c == "xmp").unwrap();
    assert_eq!(metadata[0].cells[xmp], Cell::Blob(raw_xmp));
    assert_eq!(fs::read(&source).unwrap(), original_bytes);
    assert_eq!(tree(&captured), captured_before);
}
