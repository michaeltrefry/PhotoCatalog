//! Numeric equality applies to derived relationships, never opaque retained cells.
use super::*;

fn real(value: f64) -> Cell {
    Cell::RealBits(value.to_bits())
}
fn text(value: &str) -> Cell {
    Cell::Text(value.as_bytes().to_vec())
}

#[test]
fn numeric_relationship_equality_matches_sqlite_without_rounding_integer_ids() {
    let db = Connection::open_in_memory().unwrap();
    let pairs = [
        (Cell::Integer(17913), real(17913.0), true),
        (Cell::Integer(0), real(-0.0), true),
        (
            Cell::Integer(9_007_199_254_740_992),
            real(9_007_199_254_740_992.0),
            true,
        ),
        (
            Cell::Integer(9_007_199_254_740_993),
            real(9_007_199_254_740_992.0),
            false,
        ),
        (
            Cell::Integer(9_007_199_254_740_994),
            real(9_007_199_254_740_994.0),
            true,
        ),
        (
            Cell::Integer(i64::MIN),
            real(-9_223_372_036_854_775_808.0),
            true,
        ),
        (
            Cell::Integer(i64::MAX),
            real(9_223_372_036_854_775_808.0),
            false,
        ),
        (
            Cell::Integer(i64::MIN),
            real(-9_223_372_036_854_777_856.0),
            false,
        ),
        (
            Cell::Integer(9_223_372_036_854_774_784),
            real(9_223_372_036_854_774_784.0),
            true,
        ),
        (Cell::Integer(1), real(1.25), false),
        (Cell::Integer(1), text("1"), false),
        (text("1"), Cell::Blob(b"1".to_vec()), false),
        (Cell::Integer(1), real(f64::INFINITY), false),
        (Cell::Integer(-1), real(f64::NEG_INFINITY), false),
    ];
    for (left, right, expected) in pairs {
        let before = serde_json::to_vec(&(&left, &right)).unwrap();
        let native: bool = db
            .query_row("SELECT ?1 = ?2", params![&left, &right], |r| r.get(0))
            .unwrap();
        assert_eq!(native, expected, "native {left:?} {right:?}");
        assert_eq!(
            relationship_key(&left).unwrap() == relationship_key(&right).unwrap(),
            expected,
            "derived {left:?} {right:?}"
        );
        assert_eq!(serde_json::to_vec(&(&left, &right)).unwrap(), before);
    }
    for value in [
        real(f64::NAN),
        real(f64::INFINITY),
        real(f64::NEG_INFINITY),
        real(1.25),
        real(9_223_372_036_854_775_808.0),
        text("17913"),
        Cell::Blob(vec![0xff, 0]),
    ] {
        assert_eq!(
            relationship_key(&value).unwrap(),
            serde_json::to_string(&value).unwrap()
        );
    }
    for value in [Cell::Null, Cell::Integer(0), real(0.0), real(-0.0)] {
        assert!(absent_reference(&value));
    }
    for value in [
        text("0"),
        Cell::Blob(b"0".to_vec()),
        real(f64::NAN),
        real(0.5),
    ] {
        assert!(!absent_reference(&value));
    }
}

fn plan(temp: &Path) -> Plan {
    let plan = Plan::create(&temp.join("plan")).unwrap();
    let manifest = Manifest {
        protocol: PROTOCOL,
        request: super::super::capture::Request {
            source: NativePath::from_path(&temp.join("unused-source")),
            output: NativePath::from_path(&temp.join("unused-capture")),
            include_auxiliary: false,
            closed_application_evidence: None,
            limits: Limits::default(),
        },
        state: "captured".into(),
        raw_byte_retention: "complete".into(),
        sqlite_consistency: "synthetic projection fixture".into(),
        application_consistency: "unverified".into(),
        cooperative_lock_protocol: "fixture".into(),
        artifacts: vec![],
        companion_inventory: vec![],
        absent_companions: vec![],
        issues: vec![],
        wal: None,
        logical_blake3: None,
        logical_revision: None,
        revision_id: Some("r".into()),
    };
    plan.db.execute("INSERT INTO captures(revision,lineage,path,manifest,stage,schema_version) VALUES('r','lineage','unused',?,'pending','1000000')",
        [serde_json::to_string(&manifest).unwrap()]).unwrap();
    plan
}
fn entity(plan: &Plan, id: &str, table: &str, values: &[(&str, Cell)]) {
    let columns: Vec<_> = values.iter().map(|(name, _)| name.to_string()).collect();
    let cells: Vec<_> = values.iter().map(|(_, cell)| cell.clone()).collect();
    plan.db.execute("INSERT INTO rows(revision,source_id,table_name,key_json,cells_json) VALUES('r',?,?,?,?)",
        params![id,table,serde_json::to_string(id).unwrap(),serde_json::to_string(&cells).unwrap()]).unwrap();
    retain_entity(&plan.db, "r", id, table, &columns, &cells).unwrap();
    retain_references(&plan.db, "r", id, table, &columns, &cells).unwrap();
}
fn raw_state(plan: &Plan) -> Vec<(String, String)> {
    plan.db
        .prepare("SELECT source_id,cells_json FROM rows ORDER BY sequence")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn packet_fact(plan: &Plan, owner: &str) {
    plan.db.execute("INSERT INTO packets(revision,source_id,origin,raw_digest,raw,detail) VALUES('r',?,'catalog','opaque',x'00ff','retained')",[owner]).unwrap();
    plan.db.execute("INSERT INTO metadata_facts(revision,source_id,origin,packet_digest,field,value_json) VALUES('r',?,'catalog','opaque','rating','5')",[owner]).unwrap();
}

#[test]
fn real_relations_resolve_develop_path_metadata_and_zero_master_without_changing_raw_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let mut plan = plan(temp.path());
    entity(
        &plan,
        "root",
        "AgLibraryRootFolder",
        &[
            ("id_local", Cell::Integer(1)),
            ("absolutePath", text("/controlled-never-opened/")),
        ],
    );
    entity(
        &plan,
        "folder",
        "AgLibraryFolder",
        &[
            ("id_local", Cell::Integer(2)),
            ("rootFolder", real(1.0)),
            ("parentId", real(-0.0)),
            ("pathFromRoot", text("images/")),
        ],
    );
    entity(
        &plan,
        "file",
        "AgLibraryFile",
        &[
            ("id_local", Cell::Integer(3)),
            ("folder", real(2.0)),
            ("idx_filename", text("source.png")),
        ],
    );
    entity(
        &plan,
        "image",
        "Adobe_images",
        &[
            ("id_local", Cell::Integer(17903)),
            ("id_global", text("opaque-global")),
            ("rootFile", real(3.0)),
            ("masterImage", real(-0.0)),
            ("developSettingsIDCache", real(17913.0)),
        ],
    );
    entity(
        &plan,
        "develop",
        "Adobe_imageDevelopSettings",
        &[("id_local", Cell::Integer(17913)), ("image", real(17903.0))],
    );
    entity(
        &plan,
        "packet",
        "Adobe_AdditionalMetadata",
        &[("id_local", Cell::Integer(4)), ("image", real(17903.0))],
    );
    packet_fact(&plan, "packet");
    let before = raw_state(&plan);
    let globals: Vec<(String, Option<String>)> = plan
        .db
        .prepare("SELECT source_id,global_key FROM entities ORDER BY source_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    plan.reconcile("r").unwrap();
    assert_eq!(raw_state(&plan), before);
    assert_eq!(
        unique_target(
            &plan.db,
            "r",
            "image",
            "developSettingsIDCache",
            "Adobe_imageDevelopSettings"
        )
        .unwrap()
        .as_deref(),
        Some("develop")
    );
    let issues = plan.issues("r", 0, 100).unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0]["code"], "unknown_schema_version");
    assert!(issues[0]["detail"].as_str().unwrap().contains("1000000"));
    assert_eq!(
        plan.db
            .query_row("SELECT file_source_id FROM metadata_facts", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "file"
    );
    assert_eq!(
        plan.db
            .query_row("SELECT raw FROM packets", [], |r| r.get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![0, 255]
    );
    assert_eq!(plan.paths("r", 0, 10).unwrap()[0]["state"], "pending");
    let path: String = plan
        .db
        .query_row("SELECT inspection_path FROM paths", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<NativePath>(&path).unwrap(),
        NativePath::UnixBytes(b"/controlled-never-opened/images/source.png".to_vec())
    );
    let after: Vec<(String, Option<String>)> = plan
        .db
        .prepare("SELECT source_id,global_key FROM entities ORDER BY source_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after, globals);
    assert_eq!(
        plan.db
            .query_row(
                "SELECT count(*) FROM references_out WHERE field IN ('masterImage','parentId')",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn mixed_type_duplicate_targets_never_choose_a_hierarchy_path_or_metadata_owner() {
    let temp = tempfile::tempdir().unwrap();
    let mut plan = plan(temp.path());
    entity(
        &plan,
        "keyword-a",
        "AgLibraryKeyword",
        &[("id_local", Cell::Integer(1)), ("parent", real(2.0))],
    );
    entity(
        &plan,
        "keyword-b",
        "AgLibraryKeyword",
        &[("id_local", Cell::Integer(2)), ("parent", Cell::Integer(1))],
    );
    entity(
        &plan,
        "keyword-c",
        "AgLibraryKeyword",
        &[("id_local", real(2.0)), ("parent", Cell::Null)],
    );
    entity(
        &plan,
        "folder-a",
        "AgLibraryFolder",
        &[("id_local", Cell::Integer(2)), ("parentId", Cell::Null)],
    );
    entity(
        &plan,
        "folder-b",
        "AgLibraryFolder",
        &[("id_local", real(2.0)), ("parentId", real(-0.0))],
    );
    entity(
        &plan,
        "file",
        "AgLibraryFile",
        &[
            ("id_local", Cell::Integer(3)),
            ("folder", real(2.0)),
            ("idx_filename", text("kept.png")),
        ],
    );
    entity(
        &plan,
        "image-a",
        "Adobe_images",
        &[
            ("id_local", Cell::Integer(7)),
            ("rootFile", Cell::Integer(3)),
        ],
    );
    entity(
        &plan,
        "image-b",
        "Adobe_images",
        &[("id_local", real(7.0)), ("rootFile", real(3.0))],
    );
    entity(
        &plan,
        "packet",
        "Adobe_AdditionalMetadata",
        &[("id_local", Cell::Integer(8)), ("image", real(7.0))],
    );
    packet_fact(&plan, "packet");
    let before = raw_state(&plan);
    plan.reconcile("r").unwrap();
    assert_eq!(raw_state(&plan), before);
    assert!(
        unique_target(&plan.db, "r", "packet", "image", "Adobe_images")
            .unwrap()
            .is_none()
    );
    assert!(
        entity_fields(&plan.db, "r", "AgLibraryFolder", &real(2.0))
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    let issues = plan.issues("r", 0, 100).unwrap();
    assert_eq!(
        issues
            .iter()
            .filter(|i| i["code"] == "duplicate_local_id")
            .count(),
        6
    );
    assert!(
        issues
            .iter()
            .any(|i| i["code"] == "ambiguous_hierarchy_reference" && i["source_id"] == "keyword-a")
    );
    assert!(!issues.iter().any(|i| i["code"] == "hierarchy_cycle"));
    assert!(
        issues
            .iter()
            .any(|i| i["code"] == "ambiguous_metadata_owner")
    );
    assert_eq!(plan.paths("r", 0, 10).unwrap()[0]["state"], "unresolved");
    assert!(
        plan.db
            .query_row("SELECT file_source_id FROM metadata_facts", [], |r| r
                .get::<_, Option<String>>(0))
            .unwrap()
            .is_none()
    );
}

#[test]
fn exact_numeric_hierarchy_edges_detect_cycles_and_null_zero_keys_stay_absent() {
    let temp = tempfile::tempdir().unwrap();
    let mut plan = plan(temp.path());
    entity(
        &plan,
        "one",
        "AgLibraryCollection",
        &[("id_local", Cell::Integer(1)), ("parent", real(2.0))],
    );
    entity(
        &plan,
        "two",
        "AgLibraryCollection",
        &[("id_local", real(2.0)), ("parent", Cell::Integer(1))],
    );
    for (id, cell) in [
        ("null", Cell::Null),
        ("zero", real(-0.0)),
        ("int-zero", Cell::Integer(0)),
    ] {
        entity(
            &plan,
            id,
            "AgLibraryCollection",
            &[("id_local", cell.clone()), ("parent", cell)],
        );
    }
    plan.reconcile("r").unwrap();
    let issues = plan.issues("r", 0, 100).unwrap();
    assert_eq!(
        issues
            .iter()
            .filter(|i| i["code"] == "hierarchy_cycle")
            .count(),
        2
    );
    assert!(
        !issues
            .iter()
            .any(|i| i["code"] == "duplicate_local_id" || i["code"] == "dangling_reference")
    );
    assert_eq!(
        plan.db
            .query_row(
                "SELECT count(*) FROM entities WHERE local_key IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
}
