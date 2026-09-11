//! Executes the production SQL, with independent expected cursors and typed values.
use super::*;
use rusqlite::{StatementStatus, params_from_iter, types::Value};

fn cells() -> Vec<Cell> {
    vec![
        Cell::Null,
        Cell::Integer(-42),
        Cell::RealBits((-0.0_f64).to_bits()),
        Cell::RealBits(f64::NAN.to_bits()),
        Cell::Text(vec![0xff, 0, 0xfe]),
        Cell::Blob(vec![0, 255, 1]),
    ]
}
fn populate(plan: &mut Plan, count: i64) {
    let transaction = plan.db.transaction().unwrap();
    for revision in ["selected", "noise"] {
        for name in ["common", "rare"] {
            transaction.execute("INSERT INTO tables(revision,name,columns_json,key_json,schema_json,category,state,cursor,retained) VALUES(?,?, '[\"n\",\"i\",\"negative_zero\",\"nan\",\"text\",\"blob\"]','[]','{}','retained_only','complete','preserved cursor',?)",params![revision,name,count]).unwrap();
        }
        transaction.execute("INSERT INTO captures(revision,lineage,path,manifest,stage,evidence_revision) VALUES(?, 'lineage','retained capture path','{}','paths_pending',7)",[revision]).unwrap();
    }
    let encoded = serde_json::to_string(&cells()).unwrap();
    for i in 1..=count {
        for (revision, sequence) in [("selected", 2 * i - 1), ("noise", 2 * i)] {
            let source = format!("{revision}-{:06}", count - i);
            let table = if i == count - 1 { "rare" } else { "common" };
            let key = serde_json::to_string(&vec![Cell::Integer(i)]).unwrap();
            transaction
                .execute(
                    "INSERT INTO rows VALUES(?,?,?,?,?,?)",
                    params![sequence, revision, source, table, key, encoded],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO issues VALUES(?,?,?,'code','preserved detail')",
                    params![sequence, revision, source],
                )
                .unwrap();
            transaction.execute("INSERT INTO packets VALUES(?,?,?,'catalog','digest',x'00ff',x'ff00aa','preserved packet')",params![sequence,revision,source]).unwrap();
            transaction
                .execute(
                    "INSERT INTO paths VALUES(?,?,?,'original','{}','pending','{}')",
                    params![sequence, revision, source],
                )
                .unwrap();
        }
    }
    transaction.execute("INSERT INTO metadata_facts VALUES('selected','source','file','catalog','digest','rating','5')",[]).unwrap();
    transaction
        .execute("INSERT INTO inventories VALUES('inventory','{}')", [])
        .unwrap();
    transaction
        .execute(
            "INSERT INTO family_assignments VALUES('selected','family','evidence')",
            [],
        )
        .unwrap();
    transaction.execute("INSERT INTO family_choices VALUES('family','selected','evidence digest','explicit choice')",[]).unwrap();
    transaction.commit().unwrap();
}
fn measured(
    db: &Connection,
    sql: &str,
    values: &[Value],
) -> (Vec<Vec<Cell>>, i32, i32, Vec<String>) {
    let mut statement = db.prepare(sql).unwrap();
    let columns = statement.column_count();
    let result = statement
        .query_map(params_from_iter(values), |row| {
            Ok((0..columns)
                .map(|i| Cell::from_sql(row.get_ref(i).unwrap(), 8192).unwrap())
                .collect())
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<Vec<Cell>>>>()
        .unwrap();
    let steps = statement.get_status(StatementStatus::VmStep);
    let sorts = statement.get_status(StatementStatus::Sort);
    let mut explain = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let plan = explain
        .query_map(params_from_iter(values), |r| r.get(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    (result, steps, sorts, plan)
}
#[test]
fn paging_work_is_bounded_for_interleaved_revisions_and_sparse_tables() {
    let mut receipts = vec![];
    for count in [100, 1000, 10000] {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
        populate(&mut plan, count);
        for after in [0, count, 2 * count] {
            for filter in [None, Some("common"), Some("rare"), Some("missing")] {
                let mut parameters = vec![Value::Text("selected".into()), Value::Integer(after)];
                let sql = if let Some(table) = filter {
                    parameters.push(Value::Text(table.into()));
                    TABLE_ROWS_PAGE
                } else {
                    ROWS_PAGE
                };
                parameters.push(Value::Integer(20));
                let (actual, steps, sorts, explain) = measured(&plan.db, sql, &parameters);
                let expected: Vec<i64> = (1..=count)
                    .filter(|i| {
                        2 * i - 1 > after
                            && match filter {
                                None => true,
                                Some("rare") => *i == count - 1,
                                Some("common") => *i != count - 1,
                                _ => false,
                            }
                    })
                    .take(20)
                    .collect();
                assert_eq!(
                    actual.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                    expected
                        .iter()
                        .map(|i| Cell::Integer(2 * i - 1))
                        .collect::<Vec<_>>()
                );
                assert_eq!(sorts, 0, "{explain:?}");
                assert!(
                    steps < 1000,
                    "work grew with unrelated rows: count={count} filter={filter:?} after={after}: {steps}, {explain:?}"
                );
                assert!(
                    explain
                        .iter()
                        .any(|line| line.contains(if filter.is_some() {
                            "source_rows"
                        } else {
                            "rows_revision_sequence"
                        }) && line.contains("sequence>?")),
                    "{explain:?}"
                );
                // Full API comparison covers every returned field, including bit-exact floats
                // and invalid-UTF8 text/blob bytes, independently of SQL row equality.
                let rows = plan.rows("selected", filter, after, 20).unwrap();
                assert_eq!(rows.len(), expected.len());
                for (row, i) in rows.iter().zip(&expected) {
                    assert_eq!(row.sequence, 2 * i - 1);
                    assert_eq!(row.source_id, format!("selected-{:06}", count - i));
                    assert_eq!(row.revision_id, "selected");
                    assert_eq!(row.table, if *i == count - 1 { "rare" } else { "common" });
                    assert_eq!(row.source_key, vec![Cell::Integer(*i)]);
                    assert_eq!(
                        row.columns,
                        ["n", "i", "negative_zero", "nan", "text", "blob"]
                    );
                    assert_eq!(row.cells, cells());
                    assert_eq!(row.semantics, "retained_only");
                }
                receipts.push(serde_json::json!({"count":count,"surface":"rows","after":after,"filter":filter,"returned":actual.len(),"vm_step":steps,"sort":sorts,"plan":explain}));
            }
            for (name, sql, index) in [
                ("issues", ISSUES_PAGE, "issues_revision_sequence"),
                ("packets", PACKETS_PAGE, "packets_revision_sequence"),
                ("paths", PATHS_PAGE, "paths_revision_sequence"),
            ] {
                let parameters = [
                    Value::Text("selected".into()),
                    Value::Integer(after),
                    Value::Integer(20),
                ];
                let (actual, steps, sorts, explain) = measured(&plan.db, sql, &parameters);
                let expected: Vec<i64> = (1..=count)
                    .map(|i| 2 * i - 1)
                    .filter(|i| *i > after)
                    .take(20)
                    .collect();
                assert_eq!(
                    actual.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                    expected
                        .iter()
                        .copied()
                        .map(Cell::Integer)
                        .collect::<Vec<_>>()
                );
                assert_eq!(sorts, 0);
                assert!(steps < 1000, "{name} {count} {after}: {steps}");
                assert!(
                    explain
                        .iter()
                        .any(|line| line.contains(index) && line.contains("sequence>?")),
                    "{explain:?}"
                );
                let page = match name {
                    "issues" => plan.issues("selected", after, 20),
                    "packets" => plan.packets("selected", after, 20),
                    _ => plan.paths("selected", after, 20),
                }
                .unwrap();
                assert_eq!(page.len(), expected.len());
                for (row, seq) in page.iter().zip(expected) {
                    assert_eq!(row["sequence"], seq);
                }
                receipts.push(serde_json::json!({"count":count,"surface":name,"after":after,"returned":actual.len(),"vm_step":steps,"sort":sorts,"plan":explain}));
            }
        }
    }
    eprintln!(
        "{}",
        serde_json::json!({"scope":"native production statement-work regression, not latency qualification","sqlite_version":rusqlite::version(),"cases":receipts})
    );
}

// Logical state includes all tables and every SQLite type/field, not only counts.
fn logical_state(db: &Connection) -> Vec<(String, Vec<Vec<Cell>>)> {
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let sql = format!(
                "SELECT rowid,* FROM \"{}\" ORDER BY rowid",
                table.replace('"', "\"\"")
            );
            let rows = measured(db, &sql, &[]).0;
            (table, rows)
        })
        .collect()
}
fn disk_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_str().unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}
#[test]
fn legacy_plans_are_rejected_before_write_open_with_all_evidence_unchanged() {
    for version in [1, 2] {
        for live_wal in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("plan");
            let mut plan = Plan::create(&root).unwrap();
            populate(&mut plan, 20);
            plan.db
                .pragma_update(None, "user_version", version)
                .unwrap();
            plan.db
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            if live_wal {
                plan.db.execute("UPDATE packets SET detail='uncheckpointed retained packet detail' WHERE sequence=1", []).unwrap();
                assert!(
                    fs::metadata(root.join("inspection.sqlite3-wal"))
                        .unwrap()
                        .len()
                        > 32
                );
                assert!(root.join("inspection.sqlite3-shm").is_file());
                let before = disk_bytes(&root);
                let error = Plan::open(&root)
                    .err()
                    .expect("legacy semantic keys must not be admitted");
                assert!(
                    error
                        .to_string()
                        .contains("create a new derived plan from preserved captures")
                );
                assert_eq!(disk_bytes(&root), before);
            } else {
                drop(plan);
                let before = disk_bytes(&root);
                let error = Plan::open(&root)
                    .err()
                    .expect("legacy semantic keys must not be admitted");
                assert!(
                    error
                        .to_string()
                        .contains("create a new derived plan from preserved captures")
                );
                assert_eq!(disk_bytes(&root), before);
            }
        }
    }
}
#[test]
fn current_reopen_preserves_logical_state_and_schema_without_migration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plan");
    let mut plan = Plan::create(&root).unwrap();
    populate(&mut plan, 20);
    let before = logical_state(&plan.db);
    let schema: i64 = plan
        .db
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    plan.db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(plan);
    let plan = Plan::open(&root).unwrap();
    assert_eq!(
        plan.db
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        plan.db
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        schema
    );
    assert_eq!(logical_state(&plan.db), before);
    assert_eq!(
        plan.rows("selected", Some("rare"), 0, 1000).unwrap().len(),
        1
    );
}

#[test]
fn sparse_conflicts_scan_candidates_but_deep_empty_tail_seeks_without_order_sort() {
    let mut receipts = vec![];
    for count in [100, 1000, 10000] {
        let temp = tempfile::tempdir().unwrap();
        let plan = Plan::create(&temp.path().join("plan")).unwrap();
        let transaction = plan.db.unchecked_transaction().unwrap();
        for i in 1..=count {
            for (revision, sequence) in [("selected", 2 * i - 1), ("noise", 2 * i)] {
                transaction.execute("INSERT INTO metadata_facts(rowid,revision,source_id,file_source_id,origin,packet_digest,field,value_json) VALUES(?,?,?,?, 'catalog','digest','rating',?)",params![sequence,revision,format!("source-{:06}",count-i),format!("file-{}",if i==count{count-1}else{i}),if i==count{"5"}else{"0"}]).unwrap();
            }
        }
        transaction.commit().unwrap();
        for after in [0, 2 * count - 5, 2 * count] {
            let parameters = [
                Value::Text("selected".into()),
                Value::Integer(after),
                Value::Integer(20),
            ];
            let (actual, steps, sorts, explain) = measured(&plan.db, CONFLICTS_PAGE, &parameters);
            let expected: Vec<_> = [2 * count - 3, 2 * count - 1]
                .into_iter()
                .filter(|sequence| *sequence > after)
                .collect();
            assert_eq!(
                actual.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                expected
                    .iter()
                    .copied()
                    .map(Cell::Integer)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                sorts, 0,
                "COUNT(DISTINCT) may use a temp set, but ordering must not sort"
            );
            assert!(
                explain
                    .iter()
                    .any(|line| line.contains("facts_revision") && line.contains("rowid>?")),
                "{explain:?}"
            );
            if after == 0 {
                assert!(
                    i64::from(steps) > count * 20 && i64::from(steps) < count * 40 + 100,
                    "sparse matches require candidate inspection: {steps}"
                );
            } else {
                assert!(
                    steps < 200,
                    "deep/empty seek must skip preceding facts: {steps}"
                );
            }
            assert_eq!(
                plan.metadata_conflicts("selected", after, 20)
                    .unwrap()
                    .len(),
                expected.len()
            );
            receipts.push(serde_json::json!({"count":count,"after":after,"matches":expected.len(),"vm_step":steps,"sort":sorts,"plan":explain}));
        }
    }
    eprintln!(
        "{}",
        serde_json::json!({"scope":"rare conflict candidate scanning remains proportional; ordered navigation repair only","sqlite_version":rusqlite::version(),"cases":receipts})
    );
}

#[test]
fn pending_and_packet_queues_skip_completed_prefixes_and_preserve_upgrade_order() {
    let mut receipts = vec![];
    for count in [100, 1000, 10000] {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = Plan::create(&temp.path().join("plan")).unwrap();
        populate(&mut plan, count);
        // Large completed prefix contains metadata-only packet markers. Only the
        // final two selected entries remain pending; noise revisions interleave.
        plan.db.execute("UPDATE paths SET state='complete',evidence=CASE WHEN sequence%3=0 THEN '{\"embedded_sidecar_xmp\":\"uninspected\"}' ELSE '{}' END",[]).unwrap();
        plan.db
            .execute(
                "UPDATE paths SET state='pending' WHERE revision='selected' AND sequence>=?",
                [2 * count - 3],
            )
            .unwrap();
        let mut pending_max = 0;
        for expected in [2 * count - 3, 2 * count - 1] {
            let parameters = [Value::Text("selected".into()), Value::Integer(1)];
            let (rows, steps, sorts, explain) = measured(&plan.db, PATH_QUEUE, &parameters);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Cell::Integer(expected));
            assert!(steps < 40);
            assert_eq!(sorts, 0);
            assert!(
                explain
                    .iter()
                    .any(|line| line.contains("paths_pending_queue"))
            );
            pending_max = pending_max.max(steps);
            let (_, steps, _, _) = measured(&plan.db, PATH_PENDING, &parameters[..1]);
            assert!(steps < 40);
            // Mirrors metadata-only inspection: pending is removed, packet work
            // remains admitted by its marker; other state/evidence is untouched.
            plan.db.execute("UPDATE paths SET state='available_packets_uninspected',evidence='{\"embedded_sidecar_xmp\":\"uninspected\"}' WHERE sequence=?",[expected]).unwrap();
        }
        let parameters = [Value::Text("selected".into()), Value::Integer(20)];
        let (rows, steps, _, _) = measured(&plan.db, PATH_QUEUE, &parameters);
        assert!(rows.is_empty());
        assert!(steps < 40);
        assert_eq!(
            measured(&plan.db, PATH_PENDING, &parameters[..1]).0,
            vec![vec![Cell::Integer(0)]]
        );
        let expected: Vec<_> = (1..=count)
            .map(|i| 2 * i - 1)
            .filter(|sequence| sequence % 3 == 0 || *sequence >= 2 * count - 3)
            .collect();
        let mut actual = vec![];
        let mut packet_max = 0;
        let mut batches = 0;
        loop {
            let (rows, steps, sorts, explain) = measured(&plan.db, PATH_PACKET_QUEUE, &parameters);
            assert!(
                steps < 200,
                "packet queue must skip completed prefix: {count} {steps}"
            );
            assert_eq!(sorts, 0);
            assert!(
                explain
                    .iter()
                    .any(|line| line.contains("paths_packet_queue"))
            );
            packet_max = packet_max.max(steps);
            if rows.is_empty() {
                break;
            }
            batches += 1;
            let transaction = plan.db.unchecked_transaction().unwrap();
            for row in rows {
                let Cell::Integer(sequence) = row[0] else {
                    panic!("expected sequence")
                };
                actual.push(sequence);
                transaction.execute("UPDATE paths SET state='available_packets_retained',evidence='{\"packet_gaps\":false}' WHERE sequence=?",[sequence]).unwrap();
            }
            transaction.commit().unwrap();
        }
        assert_eq!(actual, expected);
        assert_eq!(
            plan.db
                .query_row(
                    "SELECT count(*) FROM paths WHERE revision='noise'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            count
        );
        receipts.push(serde_json::json!({"count":count,"packet_batches":batches,"pending_max_vm":pending_max,"packet_max_vm":packet_max,"processed":actual.len()}));
    }
    eprintln!(
        "{}",
        serde_json::json!({"scope":"repeated queue admission work, no original file access or latency qualification","sqlite_version":rusqlite::version(),"cases":receipts})
    );
}

#[test]
fn current_schema_rejects_missing_or_replaced_queue_index_before_mutation() {
    for replacement in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("plan");
        let plan = Plan::create(&root).unwrap();
        plan.db
            .execute_batch("DROP INDEX paths_pending_queue")
            .unwrap();
        if replacement {
            plan.db
                .execute_batch("CREATE INDEX paths_pending_queue ON paths(revision,sequence)")
                .unwrap();
        }
        plan.db
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(plan);
        let path = root.join("inspection.sqlite3");
        let before = fs::read(&path).unwrap();
        let error = Plan::open(&root).err().expect("damaged schema must fail");
        assert!(error.to_string().contains("paging index"), "{error:#}");
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
