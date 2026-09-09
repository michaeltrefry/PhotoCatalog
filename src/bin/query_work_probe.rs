//! Read-only bundled-SQLite work diagnostic; no Catalog or production changes.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use rusqlite::{Connection, OpenFlags, StatementStatus, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, io::Write, path::{Path, PathBuf}};

const VERSION: u32 = 1;
const PROTOCOL: u32 = 3;
const BASE_DEEP: &str = "SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE a.sequence>? ORDER BY a.sequence LIMIT 200";
const BASE_RATING: &str = "SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash FROM assets a JOIN annotations r ON r.asset_id=a.sequence WHERE r.rating=? AND a.sequence>? ORDER BY a.sequence LIMIT 200";
const CANDIDATE_DEEP: &str = "SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash\n        FROM assets a CROSS JOIN annotations r\n        WHERE r.asset_id=a.sequence AND a.sequence>?\n        ORDER BY a.sequence LIMIT 200";
const CANDIDATE_RATING: &str = "SELECT a.sequence,a.id,a.folder_id,a.captured_at,r.rating,a.preview_hash\n        FROM annotations r CROSS JOIN assets a\n        WHERE a.sequence=r.asset_id AND r.rating=? AND r.asset_id>?\n        ORDER BY r.asset_id LIMIT 200";

#[derive(Parser)]
struct Args {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    cases: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 256)]
    memory_mib: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Case {
    workload: String,
    case_label: String,
    iteration: Option<u32>,
    cursor_percent: f64,
    parameters: Vec<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    protocol_version: u32,
    count: i64,
    cases: Vec<Case>,
}

impl Manifest {
    fn validate(&self) -> Result<()> {
        ensure!(self.protocol_version == PROTOCOL, "case protocol must be 3");
        ensure!(self.count >= 10_000 && self.count <= 10_000_000, "invalid synthetic scale");
        ensure!(self.cases.len() == 6, "exactly six cases required");
        for (index, case) in self.cases.iter().enumerate() {
            let rating = index % 2 == 1;
            ensure!(case.workload == if rating { "rating" } else { "page_deep" }, "case order/workload mismatch");
            ensure!(case.parameters.len() == if rating { 2 } else { 1 }, "parameter count mismatch");
            if rating {
                ensure!((1..=5).contains(&case.parameters[0]), "invalid rating");
            }
            let cursor = *case.parameters.last().context("missing cursor")?;
            ensure!(cursor > 0 && cursor < self.count, "cursor out of bounds");
            if index < 4 {
                let percent = if index < 2 { 50 } else { 90 };
                ensure!(case.case_label == format!("cursor_{percent}_percent") && case.iteration.is_none(), "legacy case identity mismatch");
                ensure!(cursor == self.count * percent / 100 && case.cursor_percent == percent as f64, "legacy cursor mismatch");
            } else {
                ensure!(case.case_label == "frozen_iteration_9" && case.iteration == Some(9), "iteration case identity mismatch");
                // Consume Python's exact cursor; never reimplement its floating-point arithmetic.
                ensure!(case.cursor_percent == cursor as f64 * 100.0 / self.count as f64 && case.cursor_percent > 90.0 && case.cursor_percent < 95.0, "iteration cursor metadata mismatch");
            }
            if rating {
                ensure!(cursor == self.cases[index - 1].parameters[0], "paired cursors differ");
            }
        }
        ensure!(self.cases[1].parameters[0] == self.cases[3].parameters[0], "legacy ratings differ");
        Ok(())
    }
}

fn sql(variant: &str, workload: &str) -> Result<&'static str> {
    match (variant, workload) {
        ("baseline", "page_deep") => Ok(BASE_DEEP),
        ("baseline", "rating") => Ok(BASE_RATING),
        ("sqlite_page_candidate", "page_deep") => Ok(CANDIDATE_DEEP),
        ("sqlite_page_candidate", "rating") => Ok(CANDIDATE_RATING),
        _ => anyhow::bail!("unrecognized compiled SQL selection"),
    }
}

fn immutable_uri(path: &Path) -> Result<String> {
    let path = path.canonicalize()?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = {
        let text = path.to_str().context("SQLite URI requires a Unicode path on this platform")?;
        let text = text.strip_prefix("\\\\?\\").unwrap_or(text).replace('\\', "/");
        ensure!(!text.starts_with("UNC/") && !text.starts_with("//"), "UNC snapshots are unsupported; use a local synthetic copy");
        format!("/{text}").into_bytes()
    };
    let mut uri = String::from("file://");
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}")?;
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

fn connection(path: &Path, memory_mib: u32) -> Result<(Connection, String)> {
    ensure!((1..=4096).contains(&memory_mib), "memory setting must be 1..4096 MiB");
    let canonical = path.canonicalize()?;
    ensure!(canonical.is_file(), "database must be an existing file");
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = canonical.as_os_str().to_os_string();
        sidecar.push(suffix);
        ensure!(!Path::new(&sidecar).exists(), "immutable source has a sidecar: {suffix}");
    }
    let uri = immutable_uri(&canonical)?;
    let db = Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    ensure!(db.is_readonly("main")?, "source connection is writable");
    for (name, value) in [("query_only", 1_i64), ("synchronous", 2), ("fullfsync", 1), ("foreign_keys", 1), ("cache_size", -(i64::from(memory_mib) * 1024)), ("mmap_size", 0), ("temp_store", 1), ("busy_timeout", 5000), ("wal_autocheckpoint", 1000)] {
        db.pragma_update(None, name, value)?;
    }
    // journal_mode is reported, never changed on an immutable source.
    Ok((db, uri))
}

type Record = (i64, String, i64, i64, i64, String);

fn measure(db: &Connection, variant: &str, case: &Case) -> Result<Value> {
    let query = sql(variant, &case.workload)?;
    let mut explanation = db.prepare(&format!("EXPLAIN QUERY PLAN {query}"))?;
    let plan: Vec<(i64, i64, i64, String)> = explanation.query_map(params_from_iter(&case.parameters), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    ensure!(!plan.is_empty(), "missing query plan");
    let mut statement = db.prepare(query)?;
    ensure!(statement.readonly(), "compiled query is not read-only");
    let rows: Vec<Record> = statement.query_map(params_from_iter(&case.parameters), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
    let vm_step = statement.get_status(StatementStatus::VmStep);
    let sort = statement.get_status(StatementStatus::Sort);
    let fullscan_step = statement.get_status(StatementStatus::FullscanStep);
    ensure!(vm_step > 0 && sort >= 0 && fullscan_step >= 0, "invalid/missing work counters");
    ensure!(rows.len() == 200, "query returned {} rows instead of 200", rows.len());
    let cursor = *case.parameters.last().context("missing cursor")?;
    for (offset, row) in rows.iter().enumerate() {
        ensure!(row.0 > cursor && row.1 == format!("00000000-0000-4000-8000-{:012x}", row.0), "identity/cursor mismatch");
        ensure!(offset == 0 || rows[offset - 1].0 < row.0, "unstable row order");
        if case.workload == "rating" {
            ensure!(row.4 == case.parameters[0], "rating mismatch");
        } else {
            ensure!(row.0 == cursor + offset as i64 + 1, "deep page has a gap");
        }
    }
    Ok(json!({"variant":variant,"case":case,"sql":query,"plan":plan,"rows":rows,
        "work":{"VM_STEP":vm_step,"SORT":sort,"FULLSCAN_STEP":fullscan_step},
        "counter_scope":"single prepared statement, fully consumed exactly once; FULLSCAN_STEP alone misses range scans"}))
}

fn run(args: &Args, manifest: &Manifest, receipt: &mut Value) -> Result<()> {
    manifest.validate()?;
    let before = fs::metadata(&args.db)?;
    let (db, uri) = connection(&args.db, args.memory_mib)?;
    receipt["uri"] = json!(uri);
    let source_id: String = db.query_row("SELECT sqlite_source_id()", [], |row| row.get(0))?;
    receipt["sqlite_source_id"] = json!(source_id);
    let mut settings = serde_json::Map::new();
    for name in ["query_only", "synchronous", "fullfsync", "foreign_keys", "cache_size", "mmap_size", "temp_store", "busy_timeout", "wal_autocheckpoint"] {
        let value: i64 = db.pragma_query_value(None, name, |row| row.get(0))?;
        settings.insert(name.into(), json!(value));
    }
    let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    settings.insert("journal_mode".into(), json!(journal));
    receipt["settings_actual"] = json!(settings);
    let application: i64 = db.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(application == 1_346_913_089 && schema == 1, "not a recognized synthetic benchmark database");
    let (count, maximum): (i64, i64) = db.query_row("SELECT count(*),max(sequence) FROM assets", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    ensure!(count == manifest.count && maximum == manifest.count, "snapshot scale mismatch");
    let edits: i64 = db.query_row("SELECT count(*) FROM edits", [], |row| row.get(0))?;
    let recovery: i64 = db.query_row("SELECT value FROM recovery_probe WHERE id=1", [], |row| row.get(0))?;
    ensure!(edits == 0 && recovery == 0, "snapshot is not pristine");
    for case in &manifest.cases {
        receipt["active_query"] = json!({"variant":"baseline","case":case,"sql":sql("baseline", &case.workload)?});
        let original = measure(&db, "baseline", case)?;
        receipt["queries"].as_array_mut().context("receipt queries missing")?.push(original.clone());
        receipt["active_query"] = json!({"variant":"sqlite_page_candidate","case":case,"sql":sql("sqlite_page_candidate", &case.workload)?});
        let candidate = measure(&db, "sqlite_page_candidate", case)?;
        receipt["queries"].as_array_mut().context("receipt queries missing")?.push(candidate.clone());
        ensure!(original["rows"] == candidate["rows"], "native variants return different records");
    }
    drop(db);
    let after = fs::metadata(&args.db)?;
    ensure!(before.len() == after.len() && before.modified()? == after.modified()?, "source file metadata changed");
    receipt["source_bytes"] = json!(after.len());
    receipt["source_metadata_unchanged"] = json!(true);
    receipt["active_query"] = Value::Null;
    receipt["complete"] = json!(true);
    Ok(())
}

fn main() -> Result<()> {
    execute(&Args::parse())
}

fn execute(args: &Args) -> Result<()> {
    ensure!(fs::metadata(&args.cases)?.len() <= 65_536, "case manifest too large");
    let manifest_bytes = fs::read(&args.cases)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
    manifest.validate()?;
    // Reserve the output before any database access; never overwrite a receipt.
    let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&args.output)?;
    let mut receipt = json!({"version":VERSION,"protocol_version":PROTOCOL,"complete":false,
        "diagnostic_only":true,"sqlite_version":rusqlite::version(),"memory_mib":args.memory_mib,
        "manifest":manifest,"manifest_blake3":blake3::hash(&manifest_bytes).to_hex().to_string(),
        "probe_source_blake3":blake3::hash(include_bytes!("query_work_probe.rs")).to_hex().to_string(),
        "database":args.db,"queries":[],"latency_eligibility":"not evaluated",
        "independent_python_registry_and_generator_validation":"required"});
    let result = run(args, &manifest, &mut receipt);
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    serde_json::to_writer_pretty(&mut output, &receipt)?;
    writeln!(output)?;
    output.sync_all()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        let mut cases = Vec::new();
        for (label, iteration, cursor, percent, rating) in [
            ("cursor_50_percent", None, 10_000, 50.0, 3),
            ("cursor_90_percent", None, 18_000, 90.0, 3),
            ("frozen_iteration_9", Some(9), 18_100, 90.5, 4),
        ] {
            for workload in ["page_deep", "rating"] {
                cases.push(Case {
                    workload: workload.into(),
                    case_label: label.into(),
                    iteration,
                    cursor_percent: percent,
                    parameters: if workload == "rating" { vec![rating, cursor] } else { vec![cursor] },
                });
            }
        }
        Manifest { protocol_version: PROTOCOL, count: 20_000, cases }
    }

    fn fixture(path: &Path) -> Result<()> {
        let mut db = Connection::open(path)?;
        db.execute_batch("PRAGMA application_id=1346913089; PRAGMA user_version=1;
            CREATE TABLE assets(sequence INTEGER PRIMARY KEY,id TEXT,folder_id INTEGER,captured_at INTEGER,preview_hash TEXT);
            CREATE TABLE annotations(asset_id INTEGER PRIMARY KEY REFERENCES assets(sequence),rating INTEGER);
            CREATE INDEX annotation_rating_page ON annotations(rating,asset_id);
            CREATE TABLE edits(asset_id INTEGER);
            CREATE TABLE recovery_probe(id INTEGER PRIMARY KEY,value INTEGER);
            INSERT INTO recovery_probe VALUES(1,0);")?;
        let tx = db.transaction()?;
        for sequence in 1..=20_000_i64 {
            tx.execute("INSERT INTO assets VALUES(?1,?2,?3,?4,?5)", rusqlite::params![sequence,format!("00000000-0000-4000-8000-{sequence:012x}"),sequence%9,1_600_000_000+sequence,format!("preview-{sequence}")])?;
            tx.execute("INSERT INTO annotations VALUES(?1,?2)", [sequence,sequence%5+1])?;
        }
        tx.commit()?;
        Ok(())
    }

    #[test]
    fn both_variants_return_six_fields_and_real_work_without_modifying_source() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        #[cfg(unix)]
        let database = temporary.path().join("space # percent% question? 雪.sqlite3");
        #[cfg(not(unix))]
        let database = temporary.path().join("space # percent% 雪.sqlite3");
        fixture(&database)?;
        let before = fs::read(&database)?;
        let cases_path = temporary.path().join("cases.json");
        fs::write(&cases_path, serde_json::to_vec(&manifest())?)?;
        let output = temporary.path().join("receipt.json");
        execute(&Args { db: database.clone(), cases: cases_path, output: output.clone(), memory_mib: 256 })?;
        let receipt: Value = serde_json::from_slice(&fs::read(output)?)?;
        assert_eq!(receipt["complete"], true);
        let queries = receipt["queries"].as_array().context("missing queries")?;
        assert_eq!(queries.len(), 12);
        for pair in queries.as_chunks::<2>().0 {
            assert_eq!(pair[0]["rows"], pair[1]["rows"]);
            for measured in pair {
                assert!(measured["work"]["VM_STEP"].as_i64().unwrap() > 0);
                assert!(measured["work"]["SORT"].as_i64().unwrap() >= 0);
                assert!(measured["work"]["FULLSCAN_STEP"].as_i64().unwrap() >= 0);
                assert!(!measured["plan"].as_array().unwrap().is_empty());
                let rows = measured["rows"].as_array().unwrap();
                assert_eq!(rows.len(), 200);
                let first = &rows[0];
                let sequence = first[0].as_i64().unwrap();
                assert_eq!(first.as_array().unwrap().len(), 6);
                assert_eq!(first[2], sequence % 9);
                assert_eq!(first[3], 1_600_000_000 + sequence);
                assert_eq!(first[5], format!("preview-{sequence}"));
                assert_eq!(measured["sql"], sql(measured["variant"].as_str().unwrap(), measured["case"]["workload"].as_str().unwrap())?);
            }
        }
        assert_eq!(receipt["settings_actual"]["cache_size"], -262144);
        assert_eq!(receipt["settings_actual"]["query_only"], 1);
        assert_eq!(fs::read(&database)?, before);
        let (db, uri) = connection(&database, 256)?;
        assert!(uri.contains("%23") && uri.contains("%25"));
        #[cfg(unix)]
        assert!(uri.contains("%3F"));
        assert!(db.execute("UPDATE annotations SET rating=0", []).is_err());
        assert!(db.execute_batch("CREATE TABLE mutation(x)").is_err());
        drop(db);
        assert_eq!(fs::read(&database)?, before);
        assert_eq!(fs::read_dir(temporary.path())?.count(), 3);
        Ok(())
    }

    #[test]
    fn rejects_missing_wrong_and_reordered_cases() {
        for change in 0..6 {
            let mut cases = manifest();
            match change {
                0 => { cases.cases.pop(); }
                1 => cases.cases.swap(0, 1),
                2 => cases.cases[4].iteration = Some(0),
                3 => cases.cases[2].parameters[0] -= 1,
                4 => cases.cases[5].parameters[0] = 6,
                _ => cases.cases[3].parameters[0] = 1,
            }
            assert!(cases.validate().is_err());
        }
        assert!(sql("baseline", "DELETE FROM assets").is_err());
    }

    #[test]
    fn refuses_sidecars_and_keeps_existing_output() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("fixture.sqlite3");
        fixture(&database)?;
        let sidecar = temporary.path().join("fixture.sqlite3-wal");
        fs::write(&sidecar, b"uncheckpointed")?;
        assert!(connection(&database, 256).is_err());
        let cases = temporary.path().join("cases.json");
        fs::write(&cases, serde_json::to_vec(&manifest())?)?;
        let output = temporary.path().join("existing.json");
        fs::write(&output, b"original receipt")?;
        let error = execute(&Args { db: database, cases, output: output.clone(), memory_mib: 256 }).unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(fs::read(output)?, b"original receipt");
        Ok(())
    }

    #[test]
    fn incomplete_pages_leave_an_explicit_failed_receipt() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let database = temporary.path().join("fixture.sqlite3");
        fixture(&database)?;
        let db = Connection::open(&database)?;
        db.execute("DELETE FROM annotations WHERE asset_id > 18100", [])?;
        drop(db);
        let cases = temporary.path().join("cases.json");
        fs::write(&cases, serde_json::to_vec(&manifest())?)?;
        let output = temporary.path().join("failed.json");
        assert!(execute(&Args { db: database, cases, output: output.clone(), memory_mib: 256 }).is_err());
        let receipt: Value = serde_json::from_slice(&fs::read(output)?)?;
        assert_eq!(receipt["complete"], false);
        assert!(receipt["error"].as_str().unwrap().contains("instead of 200"));
        assert!(!receipt["queries"].as_array().unwrap().is_empty());
        Ok(())
    }
}
