//! Native-engine integration probe for synthetic benchmark catalogs. No image processing.
use anyhow::{Result, ensure};
use clap::Parser;
use photocatalog::Catalog;
use serde_json::json;
use std::{path::PathBuf, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    catalog: PathBuf,
    #[arg(long)]
    count: i64,
    #[arg(long, default_value_t = 100)]
    repetitions: usize,
}
fn percentile(values: &[f64], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = (sorted.len() - 1) as f64 * fraction;
    let low = position.floor() as usize;
    let high = position.ceil() as usize;
    sorted[low] + (sorted[high] - sorted[low]) * position.fract()
}
fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.count >= 1000 && args.repetitions >= 2,
        "count must be >=1000 and repetitions >=2"
    );
    let started = Instant::now();
    let catalog = Catalog::open(&args.catalog)?;
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut times = Vec::new();
    let mut first_id = None;
    for i in 0..args.repetitions {
        let cursor = args.count / 2 + args.count * 45 * (i as i64 % 10) / 1000;
        let started = Instant::now();
        let records = catalog.browse(cursor, 200)?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        ensure!(records.len() == 200, "unexpected page size");
        for (offset, asset) in records.iter().enumerate() {
            let sequence = cursor + offset as i64 + 1;
            ensure!(
                asset.sequence == sequence
                    && asset.id == format!("00000000-0000-4000-8000-{sequence:012x}"),
                "identity/order mismatch"
            );
            ensure!(
                asset.state == "ready"
                    && asset
                        .metadata
                        .as_ref()
                        .is_some_and(|m| m.width > 0 && m.height > 0),
                "metadata mismatch"
            );
        }
        first_id.get_or_insert_with(|| records[0].id.clone());
    }
    let id = first_id.unwrap();
    drop(catalog);
    let reopened = Catalog::open(&args.catalog)?;
    ensure!(reopened.get(&id)?.id == id, "identity changed after reopen");
    let connection = rusqlite::Connection::open(args.catalog.join("catalog.sqlite3"))?;
    let mixed = native_mixed(&args.catalog, args.repetitions)?;
    let mut statement = connection.prepare("EXPLAIN QUERY PLAN SELECT sequence,id,path_display,state,metadata,error FROM assets WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
    let plans: Vec<String> = statement
        .query_map(rusqlite::params![args.count * 9 / 10, 200], |r| r.get(3))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        plans
            .iter()
            .any(|plan| plan.contains("SEARCH assets USING INTEGER PRIMARY KEY")),
        "deep-page plan must seek by rowid"
    );
    ensure!(
        !plans
            .iter()
            .any(|plan| plan.contains("SCAN") || plan.contains("TEMP B-TREE")),
        "unbounded deep-page query plan"
    );
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(),"count":args.count,"open_ms":open_ms,"n":times.len(),
        "p50_ms":percentile(&times,0.5),"p95_ms":percentile(&times,0.95),"p99_ms":percentile(&times,0.99),
        "samples_ms":times,"plans":plans,"identity_metadata_restart_verified":true,"native_mixed":mixed,
        "scope":"actual Rust Catalog browse/get JSON deserialization; synthetic metadata only"})
    );
    Ok(())
}

fn configured_connection(path: &std::path::Path) -> Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(path.join("catalog.sqlite3"))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "fullfsync", true)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "cache_size", -262144)?;
    Ok(connection)
}
fn native_mixed(path: &std::path::Path, repetitions: usize) -> Result<serde_json::Value> {
    let mut foreground = configured_connection(path)?;
    let original_count: i64 =
        foreground.query_row("SELECT max(sequence) FROM assets", [], |r| r.get(0))?;
    let background_path = path.to_path_buf();
    let (ready, receiver) = std::sync::mpsc::sync_channel(0);
    let (acknowledge, acknowledged) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || -> Result<Vec<f64>> {
        let mut background = configured_connection(&background_path)?;
        let mut samples = Vec::new();
        for iteration in 0..repetitions {
            let started = Instant::now();
            let tx =
                background.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // The foreground is released only after this import holds the write transaction.
            ready.send(())?;
            for offset in 0..32 {
                let sequence = original_count + (iteration * 32 + offset) as i64 + 1;
                let template = offset as i64 + 1;
                tx.execute("INSERT INTO assets SELECT ?1,?2,?3,?3,fingerprint,state,metadata,preview_hash,error,folder_id,captured_at,camera_id,file_bytes FROM assets WHERE sequence=?4",
                    rusqlite::params![sequence,format!("00000000-0000-4000-8000-{sequence:012x}"),format!("/synthetic/native/IMG_{sequence:012}.CR2"),template])?;
                tx.execute(
                    "INSERT INTO annotations SELECT ?1,rating FROM annotations WHERE asset_id=?2",
                    rusqlite::params![sequence, template],
                )?;
                tx.execute("INSERT INTO asset_keywords SELECT ?1,keyword_id FROM asset_keywords WHERE asset_id=?2", rusqlite::params![sequence,template])?;
                if sequence % 5 == 0 {
                    tx.execute(
                        "INSERT INTO collection_assets VALUES(?1,?2)",
                        rusqlite::params![sequence % 100, sequence],
                    )?;
                }
            }
            tx.commit()?;
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            acknowledged.recv_timeout(std::time::Duration::from_secs(30))?;
        }
        Ok(samples)
    });
    let mut ratings = Vec::new();
    let mut edits = Vec::new();
    let work = (|| -> Result<()> {
        for iteration in 0..repetitions {
            receiver.recv_timeout(std::time::Duration::from_secs(30))?;
            let sequence = 1 + iteration as i64 % original_count;
            let started = Instant::now();
            let tx =
                foreground.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if iteration % 2 == 0 {
                tx.execute(
                    "UPDATE annotations SET rating=?1 WHERE asset_id=?2",
                    rusqlite::params![(iteration % 6) as i64, sequence],
                )?;
            } else {
                tx.execute("INSERT INTO edits VALUES(?1,?2,?3) ON CONFLICT(asset_id) DO UPDATE SET revision=excluded.revision,recipe=excluded.recipe",rusqlite::params![sequence,iteration as i64,"{\"version\":1,\"exposure\":0.3}"])?;
            }
            tx.commit()?;
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            if iteration % 2 == 0 {
                ratings.push(elapsed);
                ensure!(
                    foreground.query_row(
                        "SELECT rating FROM annotations WHERE asset_id=?1",
                        [sequence],
                        |r| r.get::<_, i64>(0)
                    )? == (iteration % 6) as i64,
                    "rating readback failed"
                );
            } else {
                edits.push(elapsed);
                ensure!(
                    foreground.query_row(
                        "SELECT revision FROM edits WHERE asset_id=?1",
                        [sequence],
                        |r| r.get::<_, i64>(0)
                    )? == iteration as i64,
                    "edit readback failed"
                );
            }
            acknowledge.send(())?;
        }
        Ok(())
    })();
    drop(acknowledge);
    drop(receiver);
    let background = worker
        .join()
        .map_err(|_| anyhow::anyhow!("native importer panicked"))??;
    work?;
    ensure!(
        foreground.query_row("SELECT count(*) FROM assets", [], |r| r.get::<_, i64>(0))?
            == original_count + repetitions as i64 * 32,
        "native import count mismatch"
    );
    let stats = |values: &[f64]| json!({"n":values.len(),"p50_ms":percentile(values,0.5),"p95_ms":percentile(values,0.95),"p99_ms":percentile(values,0.99),"samples_ms":values});
    Ok(
        json!({"rating":stats(&ratings),"edit":stats(&edits),"background":stats(&background),"imported_rows":repetitions*32,
        "method":"Rust rusqlite native transactions; 32 cloned synthetic template rows plus relationships per batch; foreground released after background obtains write lock",
        "settings":{"journal_mode":"wal","synchronous":"FULL","fullfsync":true,"cache_size_kib":262144,"busy_timeout_ms":5000}}),
    )
}
