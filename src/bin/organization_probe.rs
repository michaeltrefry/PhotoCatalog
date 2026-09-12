//! Frozen synthetic projection fixture and production-query diagnostic for sc-22842.
//! Preparation intentionally bypasses image decode and metadata projection; focused
//! integration tests cover those paths. Timed search always calls Catalog::search.
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use photocatalog::{
    CURRENT_SCHEMA_VERSION, Catalog, configure_catalog_connection,
    organization::Flag,
    organization_search::{Cursor, Direction, Key, Query, SearchRow, Sort, TextLimits},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fs, io::Write, path::PathBuf, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    catalog: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Explicit migration of an owned pristine fixture copy, outside timed reads.
    MigrateFixture,
    Transitions {
        #[arg(long, default_value_t = 200)]
        repetitions: usize,
    },
    Prepare {
        #[arg(long)]
        count: i64,
    },
    Query {
        #[arg(value_enum)]
        case: Case,
        #[arg(long, default_value_t = 100)]
        repetitions: usize,
        #[arg(long, default_value_t = 3)]
        warmups: usize,
        #[arg(long, default_value_t = 0)]
        start: usize,
    },
}
#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Case {
    Browse,
    Rating,
    RatingSort,
    CaptureRating,
    FilenameReverse,
    Keyword,
    WideKeyword,
    Collection,
    Mixed,
    Text,
    TextCapture,
    CameraLens,
    DateCamera,
    LabelFlag,
    Folder,
    FolderRecursive,
    Conflicted,
}
const PROTOCOL: u32 = 2;
// Row formulas/fixture marker remain compatible with frozen S7 inputs.
const FIXTURE_PROTOCOL: u32 = 1;
const SCHEMA6_TABLES: [&str; 13] = [
    "edit_changes",
    "edit_copy_items",
    "edit_copy_jobs",
    "edit_recipe_nodes",
    "edit_redo_nodes",
    "edit_variants",
    "export_alias_directories",
    "export_alias_dirty",
    "export_alias_paths",
    "export_alias_state",
    "photo_export_blobs",
    "photo_export_items",
    "photo_export_jobs",
];
const SCHEMA7_TABLES: &[&str] = &[
    "catalog_images",
    "image_import_map",
    "image_import_reservations",
    "image_shared_events",
    "image_shared_state",
    "image_storage_events",
    "metadata_image_export_authorities",
    "metadata_image_observations",
    "metadata_image_sources",
    "migration_artifacts",
    "migration_images",
    "migration_record_lookup",
    "migration_lookup_backfill",
    "migration_file_metadata",
    "migration_runs",
    "migration_run_supplements",
    "migration_run_items",
    "migration_reconciliation",
    "migration_mapping_epoch",
    "migration_metadata",
    "migration_organization",
    "migration_evidence",
    "migration_evidence_blobs",
    "migration_evidence_chunks",
    "migration_originals",
    "migration_retained_fields",
    "migration_retained_records",
    "migration_retention",
    "organization_collection_order",
    "organization_collection_structure",
    "organization_image_relations",
    "organization_keyword_synonyms",
];
fn id(i: i64) -> String {
    format!("fixture-{i:012}")
}
fn capture(i: i64) -> String {
    format!("2024-01-{:02}T12:00:00", i % 28 + 1)
}
fn filename(i: i64) -> String {
    format!("file{i:012}.jpg")
}
fn flag(i: i64) -> &'static str {
    match i % 3 {
        0 => "reject",
        1 => "pick",
        _ => "unflagged",
    }
}
fn format(i: i64) -> &'static str {
    if i % 2 == 0 { "JPEG" } else { "DNG" }
}
fn title(i: i64) -> &'static str {
    if i % 5 == 0 {
        "blue sunset"
    } else {
        "green mountain"
    }
}
fn conflicts(i: i64) -> Vec<&'static str> {
    if i % 97 == 0 {
        vec!["gps_latitude"]
    } else {
        vec![]
    }
}
fn main() -> Result<()> {
    let args = Args::parse();
    // Reserve the receipt before any work; failed runs retain a terminal error.
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)?;
    let result = run(&args);
    let receipt = match &result {
        Ok(v) => v.clone(),
        Err(error) => {
            json!({"protocol":PROTOCOL,"catalog_schema":CURRENT_SCHEMA_VERSION,"complete":false,"error":format!("{error:#}")})
        }
    };
    serde_json::to_writer(&mut output, &receipt)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    result?;
    ensure!(
        receipt["complete"] == true,
        "one or more query samples failed; retained in receipt"
    );
    Ok(())
}
fn run(args: &Args) -> Result<serde_json::Value> {
    if matches!(args.command, Command::MigrateFixture) {
        return migrate_fixture(args);
    }
    if let Command::Prepare { count } = args.command {
        return prepare(args, count);
    }
    let mut db = Connection::open_with_flags(
        args.catalog.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    let (count, protocol): (i64, u32) = db.query_row(
        "SELECT count,protocol FROM organization_fixture WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        protocol == FIXTURE_PROTOCOL && (1000..=10_000_000).contains(&count),
        "fixture protocol/count mismatch"
    );
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version == CURRENT_SCHEMA_VERSION,
        "fixture requires explicit migration before measurement"
    );
    let epoch: i64 = db.query_row("SELECT epoch FROM organization_state WHERE id=1", [], |r| {
        r.get(0)
    })?;
    // No full-table count or hash is performed before fresh-process query timing.
    let max: i64 = db.query_row("SELECT MAX(sequence) FROM assets", [], |r| r.get(0))?;
    ensure!(max == count, "fixture high-water changed");
    let opened = Instant::now();
    let mut cat = Catalog::open(&args.catalog)?;
    let open_ms = opened.elapsed().as_secs_f64() * 1000.;
    let settings = read_settings(&mut db)?;
    drop(db);
    if let Command::Transitions { repetitions } = args.command {
        return transitions(args, count, cat, settings, repetitions);
    }
    let Command::Query {
        case,
        repetitions,
        warmups,
        start,
    } = args.command
    else {
        unreachable!()
    };
    ensure!(
        (1..=100).contains(&repetitions) && warmups <= 3 && start <= 10000,
        "invalid repetition bounds"
    );
    let query = query(case);
    let plans = cat.explain_search(&query, Some(&anchor(&query, count, epoch, start)?), 4096)?;
    let mut samples = Vec::new();
    let mut warmup_samples = Vec::new();
    let mut errors = Vec::new();
    for i in 0..warmups + repetitions {
        let iteration = start + i.saturating_sub(warmups);
        let cursor = anchor(&query, count, epoch, iteration)?;
        let sample = match measure(&mut cat, case, &query, count, &cursor, iteration) {
            Ok(sample) => {
                if let Some(error) = sample.get("error").and_then(|e| e.as_str()) {
                    errors.push(error.to_string());
                }
                sample
            }
            Err(error) => {
                let error = format!("{error:#}");
                errors.push(error.clone());
                json!({"iteration":iteration,"anchor":cursor,"error":error})
            }
        };
        if i >= warmups {
            samples.push(sample);
        } else {
            warmup_samples.push(sample);
        }
    }
    Ok(
        json!({"protocol":PROTOCOL,"catalog_schema":CURRENT_SCHEMA_VERSION,"complete":errors.is_empty(),"errors":errors,"mode":"query","count":count,"case":case,"query":query,"repetitions":repetitions,"warmups":warmups,"start":start,"open_ms":open_ms,"settings":settings,"engine_version":rusqlite::version(),"text_limits":TextLimits::default(),"plans":plans,"samples":samples,"warmup_samples":warmup_samples}),
    )
}
fn fixture_tables(db: &Connection) -> Result<Vec<String>> {
    Ok(db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}
fn fixture_data_identity(
    db: &Connection,
    tables: &[String],
    pre_image_columns: bool,
) -> Result<(String, Vec<(String, i64)>)> {
    use rusqlite::types::ValueRef;
    let mut hash = blake3::Hasher::new();
    let mut counts = Vec::new();
    for table in tables {
        hash.update(&(table.len() as u64).to_le_bytes());
        hash.update(table.as_bytes());
        let quoted = table.replace('"', "\"\"");
        let without_rowid: bool = db.query_row(
            "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
            [table],
            |r| r.get(0),
        )?;
        let columns = db
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let columns = columns
            .iter()
            .filter(|c| {
                !(pre_image_columns && table == "assets" && c.as_str() == "physical_generation")
            })
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(",");
        let filter = if pre_image_columns && table == "sqlite_sequence" {
            " WHERE name NOT IN ('catalog_images','organization_image_relations')"
        } else {
            ""
        };
        let sql = if without_rowid {
            let keys = db
                .prepare("SELECT name FROM pragma_table_info(?1) WHERE pk>0 ORDER BY pk")?
                .query_map([table], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(!keys.is_empty(), "WITHOUT ROWID table has no primary key");
            let order = keys
                .iter()
                .map(|key| format!("\"{}\"", key.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(",");
            format!("SELECT {columns} FROM \"{quoted}\"{filter} ORDER BY {order}")
        } else {
            format!("SELECT rowid,{columns} FROM \"{quoted}\"{filter} ORDER BY rowid")
        };
        let mut stmt = db.prepare(&sql)?;
        let columns = stmt.column_count();
        hash.update(&(columns as u64).to_le_bytes());
        let mut rows = stmt.query([])?;
        let mut count = 0i64;
        while let Some(row) = rows.next()? {
            count += 1;
            hash.update(b"row");
            for col in 0..columns {
                match row.get_ref(col)? {
                    ValueRef::Null => {
                        hash.update(b"null");
                    }
                    ValueRef::Integer(v) => {
                        hash.update(b"int");
                        hash.update(&v.to_le_bytes());
                    }
                    ValueRef::Real(v) => {
                        hash.update(b"real");
                        hash.update(&v.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(v) | ValueRef::Blob(v) => {
                        hash.update(if matches!(row.get_ref(col)?, ValueRef::Text(_)) {
                            b"text"
                        } else {
                            b"blob"
                        });
                        hash.update(&(v.len() as u64).to_le_bytes());
                        hash.update(v);
                    }
                }
            }
        }
        hash.update(&count.to_le_bytes());
        counts.push((table.clone(), count));
    }
    Ok((hash.finalize().to_hex().to_string(), counts))
}
fn migrate_fixture(args: &Args) -> Result<serde_json::Value> {
    let path = args.catalog.join("catalog.sqlite3");
    let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let before_schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (4..=CURRENT_SCHEMA_VERSION).contains(&before_schema),
        "migration requires fixture schema4,5 or current"
    );
    let (count, protocol): (i64, u32) = db.query_row(
        "SELECT count,protocol FROM organization_fixture WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        protocol == FIXTURE_PROTOCOL && (1000..=10_000_000).contains(&count),
        "fixture identity mismatch"
    );
    let tables_before = fixture_tables(&db)?;
    if before_schema < 6 {
        ensure!(
            !tables_before
                .iter()
                .any(|t| SCHEMA6_TABLES.contains(&t.as_str())),
            "legacy schema contains unexpected schema6 tables"
        );
    }
    if before_schema >= 6 {
        ensure!(
            SCHEMA6_TABLES
                .iter()
                .all(|t| tables_before.iter().any(|name| name.as_str() == *t)),
            "current schema missing schema6 tables"
        );
    }
    ensure!(
        SCHEMA7_TABLES
            .iter()
            .all(|t| tables_before.iter().any(|n| n == t) == (before_schema >= 7)),
        "schema7 table roster disagrees with version"
    );
    if before_schema < 7 {
        ensure!(!db.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM sqlite_sequence WHERE name IN ('catalog_images','organization_image_relations'))",[],|r|r.get(0))?, "legacy fixture has unexpected image sequence rows");
    }
    let before = fixture_data_identity(&db, &tables_before, before_schema < 7)?;
    drop(db);
    drop(Catalog::open(&args.catalog)?);
    let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tables_after = fixture_tables(&db)?;
    ensure!(
        tables_before.iter().all(|t| tables_after.contains(t)),
        "migration removed a table"
    );
    let added: Vec<_> = tables_after
        .iter()
        .filter(|t| !tables_before.contains(t))
        .cloned()
        .collect();
    let mut expected_added: Vec<String> = Vec::new();
    if before_schema < 6 {
        expected_added.extend(SCHEMA6_TABLES.iter().map(|s| s.to_string()));
    }
    if before_schema < 7 {
        expected_added.extend(SCHEMA7_TABLES.iter().map(|s| s.to_string()));
    }
    expected_added.sort();
    ensure!(
        added == expected_added,
        "unexpected migration table additions"
    );
    let added_identity = fixture_data_identity(&db, &added, false)?;
    // This is a pristine query fixture, not an export-projection benchmark.
    // Migration creates a state row and one dirty row per existing binding;
    // those rows must be verified, not incorrectly classified as empty tables.
    let bound: i64 = db.query_row("SELECT count(*) FROM storage_bindings", [], |r| r.get(0))?;
    let unbound: i64 = db.query_row("SELECT count(*) FROM assets a WHERE NOT EXISTS(SELECT 1 FROM storage_bindings b WHERE b.asset_id=a.id)", [], |r| r.get(0))?;
    let state: (i64, i64) = db.query_row("SELECT id,unbound FROM export_alias_state", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    ensure!(
        state == (1, unbound) && unbound + bound == count,
        "incorrect alias initial state"
    );
    let dirty_mismatch: bool = db.query_row("SELECT EXISTS(SELECT asset_id FROM storage_bindings EXCEPT SELECT asset_id FROM export_alias_dirty) OR EXISTS(SELECT asset_id FROM export_alias_dirty EXCEPT SELECT asset_id FROM storage_bindings)", [], |r| r.get(0))?;
    ensure!(
        !dirty_mismatch,
        "alias dirty membership differs from bindings"
    );
    let all_initial = fixture_data_identity(
        &db,
        &SCHEMA6_TABLES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        false,
    )?;
    ensure!(
        all_initial.1.iter().all(|(name, n)| *n
            == match name.as_str() {
                "export_alias_state" => 1,
                "export_alias_dirty" => bound,
                _ => 0,
            }),
        "schema6 fixture has non-initial edit/export/alias rows"
    );
    let image_initial = fixture_data_identity(
        &db,
        &SCHEMA7_TABLES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        false,
    )?;
    ensure!(
        image_initial.1.iter().all(|(name, n)| *n
            == if matches!(name.as_str(), "catalog_images" | "image_shared_state") {
                count
            } else if name == "migration_mapping_epoch" {
                1
            } else {
                0
            }),
        "schema7 fixture has non-initial image/import rows"
    );
    ensure!(
        db.query_row::<bool, _, _>(
            "SELECT count(*)=1 AND min(id)=1 AND min(epoch)=0 FROM migration_mapping_epoch",
            [],
            |r| r.get(0)
        )?,
        "migration mapping epoch is not initial"
    );
    ensure!(db.query_row::<bool,_,_>("SELECT NOT EXISTS(SELECT 1 FROM assets a LEFT JOIN catalog_images i ON i.id=a.id WHERE i.id IS NULL OR i.sequence!=a.sequence OR i.asset_id!=a.id OR i.variant_id!='master' OR i.role!='master' OR i.origin!='native' OR i.translation_state!='native' OR i.master_sequence IS NOT NULL OR i.copied_from_sequence IS NOT NULL OR i.pixel_generation!=0 OR i.applied_shared_epoch!=0 OR a.physical_generation!=a.render_generation) AND NOT EXISTS(SELECT 1 FROM image_shared_state WHERE epoch!=0)",[],|r|r.get(0))?, "image migration identities/generations changed");
    let alias_initial_state = json!({"unbound":unbound,"dirty":bound});
    let after = fixture_data_identity(&db, &tables_before, before_schema < 7)?;
    let after_schema: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let index: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE name='organization_lens_capture'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        before == after && after_schema == CURRENT_SCHEMA_VERSION,
        "fixture migration changed logical rows"
    );
    let expected =
        "CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence)";
    ensure!(index == expected, "unexpected capture index definition");
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(
        json!({"protocol":PROTOCOL,"catalog_schema":CURRENT_SCHEMA_VERSION,"mode":"migrate_fixture","complete":true,"count":count,"schema_before":before_schema,"schema_after":after_schema,"identity_scope":"pre_existing_tables","added_tables":added_identity.1,"alias_initial_state":alias_initial_state,"image_initial_state":image_initial.1,"original_columns_preserved":true,"logical_before":before.0,"logical_after":after.0,"table_counts_before":before.1,"table_counts_after":after.1,"index_sql":index,"engine_version":rusqlite::version(),"provenance":"Explicit owned-copy schema/index migration; streamed typed logical identity covers every pre-existing table, FTS shadow table and row identity; new edit/export tables and derived alias initial rows are reported separately. Not timed query work."}),
    )
}

fn measure(
    cat: &mut Catalog,
    case: Case,
    query: &Query,
    count: i64,
    cursor: &Cursor,
    iteration: usize,
) -> Result<serde_json::Value> {
    let expected = oracle(case, query, count, cursor)?;
    let began = Instant::now();
    let mut next = Some(cursor.clone());
    let mut rows = Vec::new();
    let mut chunks = Vec::new();
    loop {
        let page = cat.search(query, next.as_ref(), 200 - rows.len(), 4096)?;
        next = page.next.clone();
        chunks.push(json!({"scanned":page.scanned,"returned":page.rows.len(),"page_complete":page.page_complete,"exhausted":page.exhausted,"has_more":page.has_more,"cursor":page.next,"vm_steps":page.vm_steps,"sorts":page.sorts,"text_work":page.text_work,"elapsed_ms":page.elapsed_ms}));
        rows.extend(page.rows);
        if rows.len() == 200 || page.exhausted {
            break;
        }
        ensure!(
            next.is_some() && chunks.len() <= 10000,
            "query continuation failed or exceeded explicit safety limit"
        );
    }
    let elapsed_ms = began.elapsed().as_secs_f64() * 1000.;
    let validation = (|| -> Result<()> {
        ensure!(
            rows.len() == expected.len(),
            "wrong result size: actual {}, expected {}",
            rows.len(),
            expected.len()
        );
        for (row, sequence) in rows.iter().zip(&expected) {
            validate_row(row, *sequence)?;
        }
        Ok(())
    })();
    let error = validation.err().map(|e| format!("{e:#}"));
    Ok(
        json!({"iteration":iteration,"anchor":cursor,"elapsed_ms":elapsed_ms,"chunks":chunks,"rows":rows,"oracle_sequences":expected,"error":error}),
    )
}

fn read_settings(db: &mut Connection) -> Result<serde_json::Value> {
    // A separate diagnostic connection uses the same shared helper. Production
    // Catalog settings are independently tested in the library, not claimed here.
    configure_catalog_connection(db)?;
    let mut settings = serde_json::Map::new();
    for name in [
        "cache_size",
        "mmap_size",
        "synchronous",
        "foreign_keys",
        "temp_store",
    ] {
        settings.insert(
            name.into(),
            json!(db.pragma_query_value(None, name, |r| r.get::<_, i64>(0))?),
        );
    }
    settings.insert(
        "journal_mode".into(),
        json!(db.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))?),
    );
    Ok(json!({"diagnostic_shared_helper_connection":settings}))
}
fn query(case: Case) -> Query {
    match case {
        Case::Browse => Query::default(),
        Case::RatingSort => Query {
            sort: Sort::Rating,
            ..Query::default()
        },
        Case::Rating => Query {
            rating: Some(4),
            ..Query::default()
        },
        Case::CaptureRating => Query {
            rating: Some(4),
            sort: Sort::Capture,
            ..Query::default()
        },
        Case::FilenameReverse => Query {
            text: Some("blu suns".into()),
            sort: Sort::Filename,
            direction: Direction::Descending,
            ..Query::default()
        },
        Case::Keyword => Query {
            keyword: Some(3),
            keyword_direct: true,
            ..Query::default()
        },
        Case::WideKeyword => Query {
            keyword: Some(1010),
            keyword_direct: true,
            ..Query::default()
        },
        Case::Collection => Query {
            collection: Some("fixture-collection".into()),
            ..Query::default()
        },
        Case::Mixed => Query {
            text: Some("blue sunset".into()),
            keyword: Some(3),
            rating: Some(4),
            flag: Some(Flag::Pick),
            label: Some("red".into()),
            date_from: Some("2024-01-05".into()),
            date_until: Some("2024-01-20".into()),
            camera_make: Some("fixture".into()),
            camera: Some("camera1".into()),
            lens: Some("lens0".into()),
            format: Some("jpeg".into()),
            ..Query::default()
        },
        Case::Text => Query {
            text: Some("blue sunset".into()),
            ..Query::default()
        },
        Case::TextCapture => Query {
            text: Some("blue sunset".into()),
            lens: Some("lens0".into()),
            sort: Sort::Capture,
            ..Query::default()
        },
        Case::CameraLens => Query {
            camera: Some("camera1".into()),
            lens: Some("lens0".into()),
            format: Some("jpeg".into()),
            ..Query::default()
        },
        Case::DateCamera => Query {
            date_from: Some("2024-01-05".into()),
            date_until: Some("2024-01-20".into()),
            camera: Some("camera1".into()),
            sort: Sort::Capture,
            ..Query::default()
        },
        Case::LabelFlag => Query {
            label: Some("red".into()),
            flag: Some(Flag::Pick),
            ..Query::default()
        },
        Case::Folder => Query {
            folder: Some(3),
            ..Query::default()
        },
        Case::FolderRecursive => Query {
            folder: Some(1),
            folder_recursive: true,
            ..Query::default()
        },
        Case::Conflicted => Query {
            only_conflicted: true,
            ..Query::default()
        },
    }
}
fn anchor(query: &Query, count: i64, epoch: i64, i: usize) -> Result<Cursor> {
    let sequence = count / 2 + count * 45 * (i as i64 % 10) / 1000;
    let key = match query.sort {
        Sort::Sequence => Key::Integer(sequence),
        Sort::Filename => Key::Text(filename(sequence)),
        Sort::Rating => Key::Integer(if i.is_multiple_of(2) { 3 } else { 5 }),
        Sort::Capture => Key::Text(format!(
            "2024-01-{:02}T12:00:00",
            if query.date_until.is_some() {
                if i.is_multiple_of(2) { 14 } else { 18 }
            } else if i.is_multiple_of(2) {
                15
            } else {
                26
            }
        )),
    };
    Ok(Cursor {
        version: photocatalog::organization_search::CURSOR_VERSION,
        query_hash: blake3::hash(&serde_json::to_vec(query)?)
            .to_hex()
            .to_string(),
        epoch,
        high_water: count,
        sequence,
        key,
    })
}
fn matches(case: Case, i: i64) -> bool {
    match case {
        Case::Browse | Case::FolderRecursive | Case::RatingSort => true,
        Case::Rating | Case::CaptureRating => i % 6 == 4,
        Case::FilenameReverse | Case::Text => i % 5 == 0,
        Case::Keyword => i % 7 == 1,
        Case::WideKeyword => i % 10000 == 1000,
        Case::Collection => i % 11 == 3,
        Case::Mixed => {
            i % 5 == 0
                && i % 7 == 1
                && i % 6 == 4
                && i % 3 == 1
                && i % 2 == 0
                && i % 4 == 0
                && (4..19).contains(&(i % 28))
        }
        Case::TextCapture => i % 20 == 0,
        Case::CameraLens => i % 3 == 1 && i % 4 == 0 && i % 2 == 0,
        Case::DateCamera => i % 3 == 1 && (4..19).contains(&(i % 28)),
        Case::LabelFlag => i % 2 == 0 && i % 3 == 1,
        Case::Folder => i % 5 == 1,
        Case::Conflicted => i % 97 == 0,
    }
}
fn oracle(case: Case, query: &Query, count: i64, cursor: &Cursor) -> Result<Vec<i64>> {
    let mut result = Vec::new();
    // Arithmetic ordered streams avoid allocating or sorting the catalog in the
    // oracle. These formulas do not execute the production SQL/predicate builder.
    match query.sort {
        Sort::Sequence | Sort::Filename => {
            if query.direction == Direction::Descending {
                for i in (1..cursor.sequence).rev() {
                    if matches(case, i) {
                        result.push(i);
                        if result.len() == 200 {
                            break;
                        }
                    }
                }
            } else {
                for i in cursor.sequence + 1..=count {
                    if matches(case, i) {
                        result.push(i);
                        if result.len() == 200 {
                            break;
                        }
                    }
                }
            }
        }
        Sort::Capture => {
            let Key::Text(key) = &cursor.key else {
                anyhow::bail!("oracle cursor kind");
            };
            let day: i64 = key[8..10].parse()?;
            for d in day..=28 {
                let start = if d == day { cursor.sequence + 1 } else { 1 };
                let remainder = d - 1;
                let first = start + (remainder - start.rem_euclid(28)).rem_euclid(28);
                for i in (first..=count).step_by(28) {
                    if matches(case, i) {
                        result.push(i);
                        if result.len() == 200 {
                            return Ok(result);
                        }
                    }
                }
            }
        }
        Sort::Rating => {
            let Key::Integer(stars) = cursor.key else {
                anyhow::bail!("oracle cursor kind");
            };
            for rating in stars..=5 {
                let start = if rating == stars {
                    cursor.sequence + 1
                } else {
                    1
                };
                let first = start + (rating - start.rem_euclid(6)).rem_euclid(6);
                for i in (first..=count).step_by(6) {
                    if matches(case, i) {
                        result.push(i);
                        if result.len() == 200 {
                            return Ok(result);
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
fn validate_row(row: &SearchRow, i: i64) -> Result<()> {
    ensure!(
        row.sequence == i
            && row.asset_id == id(i)
            && row.state == "ready"
            && row.metadata_revision == 0
            && row.folder == Some(2 + i % 5)
            && row.filename == filename(i)
            && row.capture == capture(i)
            && row.camera_make == "fixture"
            && row.camera == format!("camera{}", i % 3)
            && row.lens == format!("lens{}", i % 4)
            && row.format == format(i)
            && row.rating == Some(i % 6)
            && row.flag == flag(i)
            && row.label == if i % 2 == 0 { "red" } else { "blue" }
            && row.conflicts == conflicts(i)
            && row.provenance == json!({"synthetic_fixture":FIXTURE_PROTOCOL}),
        "full production row differs from independent fixture oracle at {i}"
    );
    Ok(())
}
fn prepare(args: &Args, count: i64) -> Result<serde_json::Value> {
    ensure!(
        (1000..=10_000_000).contains(&count),
        "fixture count must be 1000..10000000"
    );
    fs::create_dir(&args.catalog).context("fixture catalog must be a new exclusive directory")?;
    let started = Instant::now();
    drop(Catalog::open(&args.catalog)?);
    let mut db = Connection::open(args.catalog.join("catalog.sqlite3"))?;
    configure_catalog_connection(&db)?;
    db.execute_batch("CREATE TABLE organization_fixture(id INTEGER PRIMARY KEY CHECK(id=1),protocol INTEGER NOT NULL,count INTEGER NOT NULL);")?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT INTO organization_folders VALUES(1,NULL,?1,'synthetic')",
        [serde_json::to_string(
            &photocatalog::storage_volume::NativePath::UnixBytes(b"/synthetic".to_vec()),
        )?],
    )?;
    for i in 0..5 {
        tx.execute(
            "INSERT INTO organization_folders VALUES(?1,1,?2,?3)",
            params![
                i + 2,
                serde_json::to_string(&photocatalog::storage_volume::NativePath::UnixBytes(
                    format!("/synthetic/folder{i}").into_bytes()
                ))?,
                format!("folder{i}")
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO organization_keywords VALUES(1,'hierarchical',NULL,'H','[\"H\"]')",
        [],
    )?;
    for i in 0..7 {
        tx.execute(
            "INSERT INTO organization_keywords VALUES(?1,'hierarchical',1,?2,?3)",
            params![
                i + 2,
                format!("Group{i}"),
                json!(["H", format!("Group{i}")]).to_string()
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO organization_keywords VALUES(9,'hierarchical',NULL,'Wide','[\"Wide\"]')",
        [],
    )?;
    for i in 0..10000 {
        tx.execute(
            "INSERT INTO organization_keywords VALUES(?1,'hierarchical',9,?2,?3)",
            params![
                i + 10,
                format!("Term{i:05}"),
                json!(["Wide", format!("Term{i:05}")]).to_string()
            ],
        )?;
    }
    tx.execute("INSERT INTO organization_collections VALUES('fixture-collection','Synthetic collection','{\"synthetic\":true}',0)",[])?;
    tx.commit()?;
    let mut logical = blake3::Hasher::new();
    for first in (1..=count).step_by(10000) {
        let tx = db.transaction()?;
        {
            let mut asset=tx.prepare("INSERT INTO assets(sequence,id,location,path_display,state,metadata,fingerprint,preview_hash) VALUES(?1,?2,?3,?4,'ready',?5,'synthetic-fingerprint','synthetic-preview-no-object')")?;
            let mut projection=tx.prepare("INSERT INTO organization_assets VALUES(?1,?2,'ready',0,?3,?4,?5,'fixture',?6,?7,?8,?9,?10,?11,?12,?13,?14)")?;
            let mut folders =
                tx.prepare("INSERT INTO organization_folder_members VALUES(?1,?2,?3)")?;
            let mut keywords =
                tx.prepare("INSERT INTO organization_keyword_members VALUES(?1,?2,?3,NULL)")?;
            let mut text = tx.prepare("INSERT INTO organization_text(rowid,text) VALUES(?1,?2)")?;
            let mut collection=tx.prepare("INSERT INTO organization_collection_members VALUES('fixture-collection',?1,'{\"synthetic\":true}')")?;
            for i in first..=count.min(first + 9999) {
                let location = format!("/synthetic/folder{}/{}", i % 5, filename(i));
                let metadata=json!({"format":format(i),"width":8,"height":8,"orientation":1,"camera_make":"fixture","camera_model":format!("camera{}",i%3),"captured_at":capture(i),"lens":format!("lens{}",i%4),"preview_source":"synthetic projection fixture: no image exists"}).to_string();
                asset.execute(params![i, id(i), location.as_bytes(), location, metadata])?;
                projection.execute(params![
                    i,
                    id(i),
                    2 + i % 5,
                    filename(i),
                    capture(i),
                    format!("camera{}", i % 3),
                    format!("lens{}", i % 4),
                    format(i),
                    i % 6,
                    flag(i),
                    if i % 2 == 0 { "red" } else { "blue" },
                    serde_json::to_string(&conflicts(i))?,
                    json!({"synthetic_fixture":FIXTURE_PROTOCOL}).to_string(),
                    title(i)
                ])?;
                folders.execute(params![1, i, false])?;
                folders.execute(params![2 + i % 5, i, true])?;
                for (keyword, direct) in [
                    (1, false),
                    (2 + i % 7, true),
                    (9, false),
                    (10 + i % 10000, true),
                ] {
                    keywords.execute(params![keyword, i, direct])?;
                }
                text.execute(params![i, title(i)])?;
                if i % 11 == 3 {
                    collection.execute([i])?;
                }
                logical.update(&i.to_le_bytes());
                logical.update(location.as_bytes());
            }
        }
        tx.execute(
            "DELETE FROM organization_dirty WHERE sequence>=?1 AND sequence<=?2",
            params![first, first + 9999],
        )?;
        tx.commit()?;
    }
    let counts: Vec<(String, i64)> = [
        "assets",
        "organization_assets",
        "organization_keyword_members",
        "organization_folder_members",
        "organization_text",
    ]
    .iter()
    .map(|table| {
        Ok((
            (*table).to_owned(),
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?,
        ))
    })
    .collect::<Result<_>>()?;
    ensure!(
        counts.iter().all(|(table, n)| *n
            == count
                * match table.as_str() {
                    "organization_keyword_members" => 4,
                    "organization_folder_members" => 2,
                    _ => 1,
                }),
        "prepared table count mismatch"
    );
    db.execute(
        "INSERT INTO organization_fixture VALUES(1,?1,?2)",
        params![FIXTURE_PROTOCOL, count],
    )?;
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let settings = read_settings(&mut db)?;
    Ok(
        json!({"protocol":PROTOCOL,"catalog_schema":CURRENT_SCHEMA_VERSION,"complete":true,"mode":"prepare","count":count,"counts":counts,"logical_blake3":logical.finalize().to_hex().to_string(),"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"settings":settings,"engine_version":rusqlite::version(),"provenance":"Synthetic normalized query fixture; directly populated production schema. Not original-photo metadata extraction or import throughput evidence."}),
    )
}

fn transitions(
    args: &Args,
    count: i64,
    mut cat: Catalog,
    settings: serde_json::Value,
    repetitions: usize,
) -> Result<serde_json::Value> {
    use photocatalog::{
        catalog_metadata::Source,
        organization::Operation,
        xmp_packets::{self, Limits},
    };
    use std::sync::{Arc, Barrier};
    ensure!(
        (2..=200).contains(&repetitions)
            && repetitions.is_multiple_of(2)
            && count >= 200 * repetitions as i64,
        "transition workload requires an even 2..200 operations and sufficient base rows"
    );
    let inputs = args
        .output
        .parent()
        .context("receipt parent")?
        .join("source-transition-inputs");
    fs::create_dir(&inputs).context("source inputs require exclusive private directory")?;
    let mut input_hashes = Vec::new();
    for i in 0..repetitions {
        let bytes = format!(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="fixture" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:source-fixture" xmp:Rating="3" xmp:Label="import{i}"><u:opaque rdf:parseType="Resource"><u:child>unchanged</u:child></u:opaque></rdf:Description></rdf:RDF>"#
        );
        let path = inputs.join(format!("{i}.xmp"));
        fs::write(&path, &bytes)?;
        input_hashes.push(blake3::hash(bytes.as_bytes()).to_hex().to_string());
    }
    let mut db = Connection::open(args.catalog.join("catalog.sqlite3"))?;
    configure_catalog_connection(&db)?;
    let tx = db.transaction()?;
    for i in count + 1..=count + 200 {
        tx.execute("INSERT INTO assets(sequence,id,location,path_display,state) VALUES(?1,?2,?3,?4,'pending')",params![i,id(i),format!("cohort-{i}").as_bytes(),format!("synthetic cohort {i}")])?;
    }
    tx.commit()?;
    drop(db);
    while cat.organization_index(1000)?.pending {}
    let mut snapshot = cat.search_session(Query::default(), 300)?;
    let background = Catalog::open(&args.catalog)?;
    let timeline = Instant::now();
    let origin_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = barrier.clone();
    let worker_inputs = inputs.clone();
    let worker = std::thread::spawn(move || {
        let mut cat = background;
        let mut samples = Vec::new();
        worker_barrier.wait();
        for i in 0..repetitions {
            let asset = id(count + 101 + (i % 100) as i64);
            let result = (|| -> Result<serde_json::Value> {
                let file = worker_inputs.join(format!("{i}.xmp"));
                let source = Source {
                    kind: "imported_catalog".into(),
                    locator: format!("transition-{}", i % 100).into_bytes(),
                    display: format!("Synthetic source {}", i % 100),
                    ambiguous: false,
                    provenance: json!({"synthetic_transition":FIXTURE_PROTOCOL}),
                };
                let inspection = xmp_packets::inspect_sidecar(&file, &Limits::default())?;
                let before = cat.render_identity(&asset)?;
                let began = Instant::now();
                let begin_ms = timeline.elapsed().as_secs_f64() * 1000.;
                let change = cat.retain_metadata(&asset, &source, &inspection)?;
                let elapsed_ms = began.elapsed().as_secs_f64() * 1000.;
                let end_ms = timeline.elapsed().as_secs_f64() * 1000.;
                let view = cat.metadata(&asset)?;
                ensure!(
                    view.fields.iter().any(|f| f.name == "label"
                        && f.value == Some(photocatalog::xmp::Value::Text(format!("import{i}")))
                        && !f.conflicted),
                    "source label not effective"
                );
                let found = cat.search(
                    &Query {
                        label: Some(format!("import{i}")),
                        ..Query::default()
                    },
                    None,
                    2,
                    2,
                )?;
                ensure!(
                    found.exhausted
                        && found.rows.len() == 1
                        && found.rows[0].asset_id == asset
                        && found.rows[0].label == format!("import{i}"),
                    "source update missing from organization index"
                );
                ensure!(
                    change.revision > before.metadata_revision,
                    "source revision did not advance"
                );
                Ok(
                    json!({"iteration":i,"asset_id":asset,"elapsed_ms":elapsed_ms,"begin_ms":begin_ms,"end_ms":end_ms,"revision_before":before.metadata_revision,"revision_after":change.revision,"selected_label":format!("import{i}"),"observation_id":change.observation_id,"models":change.model_ids}),
                )
            })();
            samples.push(match result {
                Ok(v) => v,
                Err(e) => json!({"iteration":i,"asset_id":asset,"error":format!("{e:#}")}),
            });
        }
        samples
    });
    barrier.wait();
    let mut writes = Vec::new();
    let mut browse = Vec::new();
    for i in 0..repetitions {
        let asset = id(count + 1 + (i % 100) as i64);
        let result = (|| -> Result<serde_json::Value> {
            let before = cat.render_identity(&asset)?;
            let operation = if i.is_multiple_of(2) {
                Operation::Rating {
                    value: (i % 6) as u8,
                }
            } else {
                Operation::Label {
                    value: format!("saved{i}"),
                }
            };
            let began = Instant::now();
            let begin_ms = timeline.elapsed().as_secs_f64() * 1000.;
            let revision =
                cat.organize_asset(&asset, before.metadata_revision, operation.clone())?;
            let elapsed_ms = began.elapsed().as_secs_f64() * 1000.;
            let end_ms = timeline.elapsed().as_secs_f64() * 1000.;
            let after = cat.render_identity(&asset)?;
            ensure!(
                after.metadata_revision == revision && after.generation == before.generation,
                "nonpixel revision authority mismatch"
            );
            Ok(
                json!({"iteration":i,"asset_id":asset,"operation":operation,"elapsed_ms":elapsed_ms,"begin_ms":begin_ms,"end_ms":end_ms,"revision_before":before.metadata_revision,"revision_after":revision,"pixel_generation":after.generation}),
            )
        })();
        writes.push(match result {
            Ok(v) => v,
            Err(e) => json!({"iteration":i,"asset_id":asset,"error":format!("{e:#}")}),
        });
        let result = (|| -> Result<serde_json::Value> {
            let began = Instant::now();
            let begin_ms = timeline.elapsed().as_secs_f64() * 1000.;
            let page = snapshot.next_page(200, 4096)?;
            let elapsed_ms = began.elapsed().as_secs_f64() * 1000.;
            let end_ms = timeline.elapsed().as_secs_f64() * 1000.;
            ensure!(page.rows.len() == 200, "snapshot browse page truncated");
            for (offset, row) in page.rows.iter().enumerate() {
                validate_row(row, (i * 200 + offset + 1) as i64)?;
            }
            Ok(
                json!({"iteration":i,"elapsed_ms":elapsed_ms,"begin_ms":begin_ms,"end_ms":end_ms,"page":page}),
            )
        })();
        browse.push(match result {
            Ok(v) => v,
            Err(e) => json!({"iteration":i,"error":format!("{e:#}")}),
        });
    }
    let background = worker
        .join()
        .map_err(|_| anyhow::anyhow!("metadata background worker panicked"))?;
    snapshot.close()?;
    drop(cat);
    let cat = Catalog::open(&args.catalog)?;
    let mut reopened = Vec::new();
    let mut errors = Vec::new();
    for i in repetitions.saturating_sub(100)..repetitions {
        let asset = id(count + 1 + (i % 100) as i64);
        let view = cat.metadata(&asset)?;
        let (field, value) = if i.is_multiple_of(2) {
            ("rating", (i % 6).to_string())
        } else {
            ("label", format!("saved{i}"))
        };
        let correct = view.fields.iter().any(|f| {
            f.name == field
                && f.value == Some(photocatalog::xmp::Value::Text(value.clone()))
                && !f.conflicted
        });
        if !correct {
            errors.push(format!("reopen value mismatch at {asset}"));
        }
        reopened.push(json!({"asset_id":asset,"revision":view.revision,"field":field,"value":value,"correct":correct}));
    }
    for sample in writes.iter().chain(&browse).chain(&background) {
        if let Some(error) = sample.get("error") {
            errors.push(error.to_string());
        }
    }
    let after_hashes: Vec<_> = (0..repetitions)
        .map(|i| {
            fs::read(inputs.join(format!("{i}.xmp"))).map(|b| blake3::hash(&b).to_hex().to_string())
        })
        .collect::<std::io::Result<_>>()?;
    if input_hashes != after_hashes {
        errors.push("generated source inputs changed".into());
    }
    Ok(
        json!({"protocol":PROTOCOL,"catalog_schema":CURRENT_SCHEMA_VERSION,"mode":"transitions","complete":errors.is_empty(),"errors":errors,"count":count,"repetitions":repetitions,"settings":settings,"engine_version":rusqlite::version(),"writes":writes,"snapshot_browse":browse,"source_updates":background,"reopened":reopened,"source_hashes_before":input_hashes,"source_hashes_after":after_hashes,"origin_unix_ms":origin_unix_ms,"provenance":"Separate disposable catalog copy. 200 added metadata-only cohort assets; source packet refresh is real S4 retain_metadata, not full image import or RAW processing."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arithmetic_oracle_matches_independent_small_exhaustive_sort() -> Result<()> {
        for case in Case::value_variants() {
            let query = query(*case);
            for iteration in [0, 1, 9, 19] {
                let cursor = anchor(&query, 1000, 0, iteration)?;
                let actual = oracle(*case, &query, 1000, &cursor)?;
                let mut exhaustive: Vec<i64> = (1..=1000).filter(|i| matches(*case, *i)).collect();
                match query.sort {
                    Sort::Capture => exhaustive.sort_by_key(|i| (capture(*i), *i)),
                    Sort::Rating => exhaustive.sort_by_key(|i| (i % 6, *i)),
                    _ => {}
                }
                if query.direction == Direction::Descending {
                    exhaustive.reverse();
                }
                let expected: Vec<_> = exhaustive
                    .into_iter()
                    .filter(|i| {
                        let order = match &cursor.key {
                            Key::Text(key) if query.sort == Sort::Capture => {
                                (capture(*i), *i).cmp(&(key.clone(), cursor.sequence))
                            }
                            Key::Integer(key) if query.sort == Sort::Rating => {
                                (i % 6, *i).cmp(&(*key, cursor.sequence))
                            }
                            _ => i.cmp(&cursor.sequence),
                        };
                        if query.direction == Direction::Descending {
                            order.is_lt()
                        } else {
                            order.is_gt()
                        }
                    })
                    .take(200)
                    .collect();
                ensure!(actual == expected, "case {case:?}, iteration {iteration}");
            }
        }
        Ok(())
    }
}
