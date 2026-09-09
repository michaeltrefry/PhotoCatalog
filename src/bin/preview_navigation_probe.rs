//! Fixed headless retained-page/navigation evidence through the production service.
//! Synthetic catalog preparation is explicit; no original import or render occurs.
#[path = "preview_fixture/integrated.rs"]
mod integrated;
mod preview_fixture;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use photocatalog::{Catalog, configure_catalog_connection, preview::*, storage_volume::NativePath};
use preview_fixture::{Dataset, dataset, key, read_bounded};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const VISIBLE: usize = 200;
const ASSETS: u32 = 10_000;
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Apply a bounded preview identity overlay only to an externally copied fixture.
    Overlay {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        dataset: PathBuf,
    },
    /// Untimed receipt-chain validation; never opens images or originals.
    Verify { folder: PathBuf },
    /// Seed only a new disposable catalog; the cache dataset must already exist.
    Prepare {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Run {
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long)]
        worker: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum)]
        profile: Profile,
        #[arg(long, value_enum)]
        workload: Workload,
    },
}
#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum Profile {
    Standard,
    Constrained,
}
#[derive(Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum Workload {
    Warm,
    Fresh,
    Navigation,
}
#[derive(Serialize, Deserialize)]
struct Fixture {
    #[serde(default = "small_catalog_count")]
    catalog_count: u64,
    version: u32,
    dataset: PathBuf,
    dataset_blake3: String,
    catalog: PathBuf,
    offline_originals: PathBuf,
    count: u32,
}
fn small_catalog_count() -> u64 {
    u64::from(ASSETS)
}
fn exclusive(path: &Path, value: &Value) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    Ok(())
}
fn anchor(start: Instant) -> Value {
    json!({"unix_ns":SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d|d.as_nanos().to_string()),"elapsed_ns":start.elapsed().as_nanos().to_string()})
}
fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
fn peak_rss() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let value = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss).ok()?;
        #[cfg(target_os = "macos")]
        {
            Some(value)
        }
        #[cfg(not(target_os = "macos"))]
        {
            value.checked_mul(1024)
        }
    }
    #[cfg(not(unix))]
    {
        None
    }
}
fn limits(profile: Profile) -> ServiceLimits {
    match profile {
        Profile::Standard => ServiceLimits {
            per_worker_bytes: 2_269_118_464, // Frozen Stage B cohort allowance, not a production default.
            ..ServiceLimits::default()
        },
        Profile::Constrained => ServiceLimits {
            per_worker_bytes: 2_269_118_464,
            requests: 200,
            decoded_cache_bytes: 32 * 1024 * 1024,
            encoded_staging_bytes: 8 * 1024 * 1024,
            per_worker_encoded_bytes: 4 * 1024 * 1024,
            ..ServiceLimits::default()
        },
    }
}
fn prepare(dataset_path: &Path, output: &Path) -> Result<()> {
    ensure!(
        dataset_path.is_absolute() && output.is_absolute(),
        "absolute paths required"
    );
    let data = dataset(dataset_path)?;
    ensure!(
        data.count == ASSETS && data.id_scheme == preview_fixture::IdScheme::Layout,
        "headless fixture requires the fixed 10,000-entry layout dataset"
    );
    fs::create_dir(output).context("new fixture directory required")?;
    let fixture = Fixture {
        catalog_count: u64::from(ASSETS),
        version: 1,
        dataset: fs::canonicalize(dataset_path)?,
        dataset_blake3: blake3::hash(&read_bounded(dataset_path, 1024 * 1024)?)
            .to_hex()
            .to_string(),
        catalog: output.join("catalog"),
        offline_originals: output.join("offline-originals"),
        count: ASSETS,
    };
    let started = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"started":anchor(started),"rows":0,"kind":"synthetic direct SQL seed; not an import"});
    let result = (|| -> Result<()> {
        drop(Catalog::open(&fixture.catalog)?);
        let mut db = Connection::open(fixture.catalog.join("catalog.sqlite3"))?;
        configure_catalog_connection(&db)?;
        let tx = db.transaction()?;
        for index in 0..ASSETS {
            let k = key(&data, index);
            let source = fixture.offline_originals.join(format!("{index:010}.jpg"));
            let location = match NativePath::from_path(&source) {
                NativePath::UnixBytes(bytes) => bytes,
                NativePath::WindowsWide(units) => {
                    units.into_iter().flat_map(u16::to_le_bytes).collect()
                }
            };
            tx.execute("INSERT INTO assets(sequence,id,location,path_display,fingerprint,state,metadata,preview_hash,render_generation) VALUES(?1,?2,?3,?4,?5,'ready',?6,?7,?8)",
                params![i64::from(index)+1,k.asset_id,location,source.to_string_lossy(),k.fingerprint,
                    serde_json::to_string(&data.seeds[index as usize%30].record.metadata)?,k.digest()?,k.generation as i64])?;
            receipt["rows"] = json!(index + 1);
        }
        tx.commit()?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        ensure!(
            !fixture.offline_originals.exists(),
            "offline originals unexpectedly exist"
        );
        exclusive(
            &output.join("fixture.json"),
            &serde_json::to_value(&fixture)?,
        )?;
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = anchor(started);
    exclusive(&output.join("preparation.json"), &receipt)?;
    result
}
fn load_fixture(path: &Path) -> Result<(Fixture, Dataset)> {
    let fixture: Fixture = serde_json::from_slice(&read_bounded(path, 65536)?)?;
    ensure!(
        fixture.version == 1 && fixture.count == ASSETS,
        "unexpected navigation fixture"
    );
    ensure!(
        !fixture.offline_originals.exists(),
        "offline originals must remain absent"
    );
    ensure!(
        blake3::hash(&read_bounded(&fixture.dataset, 1024 * 1024)?)
            .to_hex()
            .as_str()
            == fixture.dataset_blake3,
        "layout fixture identity changed"
    );
    let data = dataset(&fixture.dataset)?;
    ensure!(data.count == ASSETS, "headless dataset count changed");
    ensure!(
        matches!(
            (fixture.catalog_count, data.id_scheme),
            (10_000, preview_fixture::IdScheme::Layout)
                | (10_000_000, preview_fixture::IdScheme::OrganizationFixture)
        ),
        "catalog/preview identity mode mismatch"
    );
    Ok((fixture, data))
}
fn browse(catalog: &Catalog, data: &Dataset, first: u32) -> Result<Vec<photocatalog::Asset>> {
    let mut rows = catalog.browse(i64::from(first), VISIBLE.min((ASSETS - first) as usize))?;
    if rows.len() < VISIBLE {
        rows.extend(catalog.browse(0, VISIBLE - rows.len())?);
    }
    ensure!(rows.len() == VISIBLE, "visible page length mismatch");
    for (offset, row) in rows.iter().enumerate() {
        let index = (first + offset as u32) % ASSETS;
        ensure!(
            row.sequence == i64::from(index) + 1
                && row.id == key(data, index).asset_id
                && row.state == "ready",
            "catalog page oracle mismatch"
        );
    }
    Ok(rows)
}
fn verify_view(data: &Dataset, index: u32, view: &PreviewView) -> Result<()> {
    let seed = &data.seeds[index as usize % data.seeds.len()];
    ensure!(
        !view.stale && view.key.as_ref() == Some(&key(data, index)) && view.legacy_hash.is_none(),
        "wrong/stale retained key"
    );
    let pixels = view.pixels.pixels();
    ensure!(
        pixels.width() == seed.width
            && pixels.height() == seed.height
            && pixels.digest() == seed.decoded_blake3,
        "returned RGB8 differs from independently bound seed"
    );
    ensure!(view.record.is_some(), "missing retained source provenance");
    Ok(())
}
fn outcome_name(outcome: &ReadOutcome) -> &'static str {
    match outcome {
        ReadOutcome::Ready(_) => "ready",
        ReadOutcome::Missing => "missing",
        ReadOutcome::Stale => "stale",
        ReadOutcome::Failed {
            resource_limit: true,
            ..
        } => "resource_limit",
        ReadOutcome::Failed {
            resource_limit: false,
            ..
        } => "failed",
    }
}
fn read_observation(ticket: ReadTicket, index: u32, result: &ReadCompletion) -> Value {
    let error = match &result.outcome {
        ReadOutcome::Failed { message, .. } => Some(message),
        _ => None,
    };
    json!({"ticket":ticket.0,"index":index,"outcome":outcome_name(&result.outcome),"error":error,
        "queue_ms":result.queue_ms,"owner_read_ms":result.owner_read_ms,"metrics":result.metrics})
}
fn page(
    catalog: &Catalog,
    service: &mut PreviewService,
    data: &Dataset,
    clear: bool,
    row: &mut Value,
) -> Result<()> {
    ensure!(service.is_drained(), "previous consumers not drained");
    if clear {
        service.clear_decoded_cache();
    }
    let started = Instant::now();
    row["started"] = anchor(started);
    let db_start = Instant::now();
    let assets = browse(catalog, data, 0)?;
    row["db_ms"] = json!(ms(db_start));
    let enqueue = Instant::now();
    let mut pending = HashMap::new();
    for (index, asset) in assets.iter().enumerate() {
        let ticket = service.queue_read(
            catalog,
            &asset.id,
            Tier::Thumbnail,
            false,
            Priority::Foreground,
        )?;
        pending.insert(ticket, index as u32);
    }
    row["enqueue_ms"] = json!(ms(enqueue));
    row["queue_peak"] = json!(service.read_queue_usage().queued);
    let mut views = Vec::with_capacity(VISIBLE);
    while let Some(ticket) = service.tick_read(catalog) {
        let index = pending
            .remove(&ticket)
            .context("unexpected completion ticket")?;
        let result = service.take_read(ticket).context("missing completion")?;
        row["reads"]
            .as_array_mut()
            .unwrap()
            .push(read_observation(ticket, index, &result));
        match result.outcome {
            ReadOutcome::Ready(view) => views.push((index, view)),
            _ => anyhow::bail!("required visible thumbnail did not become ready"),
        }
    }
    row["wall_ms"] = json!(ms(started)); // All 200 returned RGB8 surfaces are live.
    row["finished"] = anchor(started);
    row["decoded_live_bytes"] = json!(service.decoded_live_bytes());
    row["peak_resident_bytes"] = json!(peak_rss());
    ensure!(
        pending.is_empty() && views.len() == VISIBLE,
        "incomplete page"
    );
    let verification = Instant::now();
    for (index, view) in &views {
        verify_view(data, *index, view)?;
    }
    row["verification_ms_outside_page"] = json!(ms(verification));
    row["verified_views"] = json!(views.len());
    drop(views);
    ensure!(
        service.is_drained() && service.scheduler_usage().active == 0,
        "retained page launched/left native work"
    );
    row["complete"] = json!(true);
    Ok(())
}
fn viewport_indices(viewport: u32) -> Vec<u32> {
    (0..VISIBLE as u32)
        .map(|i| (viewport * 50 + i) % ASSETS)
        .collect()
}
fn navigation(
    catalog: &Catalog,
    service: &mut PreviewService,
    data: &Dataset,
    row: &mut Value,
) -> Result<()> {
    ensure!(service.is_drained(), "previous consumers not drained");
    service.clear_decoded_cache();
    let started = Instant::now();
    row["started"] = anchor(started);
    let mut pending: HashMap<ReadTicket, u32> = HashMap::new();
    let mut views: BTreeMap<u32, Box<PreviewView>> = BTreeMap::new();
    let mut next_viewport = 0u32;
    let mut queue_peak = 0usize;
    let mut verification_ms = 0.0;
    let mut live_peak = 0u64;
    let deadline = started + Duration::from_secs(60);
    loop {
        ensure!(
            Instant::now() < deadline,
            "navigation owner deadline exceeded"
        );
        // Catch up each scheduled input, recording every overrun; never shift the trace.
        while next_viewport < 100
            && started.elapsed() >= Duration::from_millis(u64::from(next_viewport) * 50)
        {
            let expected = viewport_indices(next_viewport);
            let visible: HashSet<_> = expected.iter().copied().collect();
            let old: Vec<_> = pending
                .iter()
                .filter(|(_, index)| !visible.contains(*index))
                .map(|(ticket, index)| (*ticket, *index))
                .collect();
            for (ticket, index) in old {
                ensure!(
                    service.cancel_read(ticket),
                    "queued cancellation lost ownership"
                );
                pending.remove(&ticket);
                row["events"].as_array_mut().unwrap().push(
                    json!({"action":"cancel","ticket":ticket.0,"index":index,"at":anchor(started)}),
                );
            }
            views.retain(|index, _| visible.contains(index));
            let db_start = Instant::now();
            let assets = browse(catalog, data, expected[0])?;
            let db_ms = ms(db_start);
            let event_start = ms(started);
            for (asset, index) in assets.iter().zip(expected) {
                if views.contains_key(&index) || pending.values().any(|value| *value == index) {
                    continue;
                }
                let ticket = service.queue_read(
                    catalog,
                    &asset.id,
                    Tier::Thumbnail,
                    false,
                    Priority::Foreground,
                )?;
                pending.insert(ticket, index);
                row["events"].as_array_mut().unwrap().push(
                    json!({"action":"submit","ticket":ticket.0,"index":index,"at":anchor(started)}),
                );
            }
            row["viewports"].as_array_mut().unwrap().push(json!({"viewport":next_viewport,"scheduled_ms":next_viewport*50,"actual_ms":event_start,"overrun_ms":(event_start-f64::from(next_viewport)*50.0).max(0.0),"db_ms":db_ms,"queue":service.read_queue_usage(),"held_views":views.len()}));
            queue_peak = queue_peak.max(service.read_queue_usage().queued);
            next_viewport += 1;
        }
        if let Some(ticket) = service.tick_read(catalog) {
            let index = pending
                .remove(&ticket)
                .context("late/unowned retained completion")?;
            let result = service
                .take_read(ticket)
                .context("missing read completion")?;
            let mut observation = read_observation(ticket, index, &result);
            observation["at"] = anchor(started);
            row["reads"].as_array_mut().unwrap().push(observation);
            match result.outcome {
                ReadOutcome::Ready(view) => {
                    let verification = Instant::now();
                    verify_view(data, index, &view)?;
                    verification_ms += ms(verification);
                    views.insert(index, view);
                }
                _ => anyhow::bail!(
                    "navigation required retained thumbnail unavailable; classification retained"
                ),
            }
            live_peak = live_peak.max(service.decoded_live_bytes());
        } else if next_viewport == 100 {
            break;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    row["wall_ms"] = json!(ms(started));
    row["final_visible_ms_from_scheduled_viewport"] = json!(ms(started) - 4950.0);
    row["verification_ms_in_trace_wall"] = json!(verification_ms);
    row["queue_peak"] = json!(queue_peak);
    row["decoded_live_peak_bytes"] = json!(live_peak);
    row["peak_resident_bytes"] = json!(peak_rss());
    row["finished"] = anchor(started);
    ensure!(
        pending.is_empty() && views.keys().copied().collect::<Vec<_>>() == viewport_indices(99),
        "final visible ownership mismatch"
    );
    drop(views);
    ensure!(
        service.is_drained() && service.scheduler_usage().active == 0,
        "navigation launched/left native work"
    );
    row["complete"] = json!(true);
    Ok(())
}
fn run(
    fixture_path: &Path,
    worker: &Path,
    output: &Path,
    profile: Profile,
    workload: Workload,
) -> Result<()> {
    ensure!(
        fixture_path.is_absolute() && worker.is_absolute() && output.is_absolute(),
        "absolute paths required"
    );
    fs::create_dir(output).context("new output directory required")?;
    let started = Instant::now();
    let mut receipt = json!({"version":1,"complete":false,"started":anchor(started),"profile":profile,"workload":workload,
        "source_blake3":blake3::hash(include_bytes!("preview_navigation_probe.rs")).to_hex().to_string(),
        "fixture_module_blake3":blake3::hash(include_bytes!("preview_fixture/mod.rs")).to_hex().to_string(),
        "cargo_lock_blake3":blake3::hash(include_bytes!("../../Cargo.lock")).to_hex().to_string(),
        "renderer_identity":renderer_identity(),"pid":std::process::id(),"limits":limits(profile),"trials":[]});
    let result = (|| -> Result<()> {
        let (fixture, data) = load_fixture(fixture_path)?;
        receipt["fixture_blake3"] = json!(
            blake3::hash(&read_bounded(fixture_path, 65536)?)
                .to_hex()
                .to_string()
        );
        receipt["dataset_blake3"] = json!(fixture.dataset_blake3);
        receipt["layout"] = json!(data.store.layout);
        receipt["sqlite_version"] = json!(rusqlite::version());
        // Count/schema preflight uses only the owned fixture copy. Its memory is
        // included in process HWM, outside every page timer.
        {
            let db = Connection::open_with_flags(
                fixture.catalog.join("catalog.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let count: u64 = db.query_row("SELECT count(*) FROM assets", [], |r| r.get(0))?;
            ensure!(
                schema == 4 && count == fixture.catalog_count,
                "catalog count/schema changed"
            );
            receipt["catalog_count"] = json!(count);
            receipt["catalog_schema"] = json!(schema);
            if data.id_scheme == preview_fixture::IdScheme::OrganizationFixture {
                ensure!(
                    fixture.offline_originals == Path::new("/synthetic"),
                    "integrated offline root mismatch"
                );
                integrated::verify_offline(&db, &data)?;
                receipt["actual_preserved_source_paths_verified"] = json!(true);
            }
        }
        let catalog = Catalog::open(&fixture.catalog)?;
        let mut service = PreviewService::open(
            data.store.clone(),
            std::slice::from_ref(&fixture.offline_originals),
            worker.to_path_buf(),
            PreviewPolicy::default(),
            limits(profile),
        )?;
        ensure!(
            service.store_usage()?.objects == u64::from(ASSETS),
            "retained dataset cardinality mismatch"
        );
        let plan: Vec<(&str, u32, bool)> = match workload {
            Workload::Warm => (0..3)
                .map(|i| ("warmup", i, true))
                .chain((0..100).map(|i| ("warm", i, true)))
                .chain(std::iter::once(("hot_lru", 0, false)))
                .collect(),
            Workload::Fresh => vec![("fresh", 0, true)],
            Workload::Navigation => (0..10).map(|i| ("navigation", i, true)).collect(),
        };
        for (kind, index, clear) in plan {
            let mut row = json!({"kind":kind,"index":index,"complete":false,"reads":[],"events":[],"viewports":[]});
            let trial = if matches!(workload, Workload::Navigation) {
                navigation(&catalog, &mut service, &data, &mut row)
            } else {
                page(&catalog, &mut service, &data, clear, &mut row)
            };
            if let Err(error) = &trial {
                row["error"] = json!(format!("{error:#}"));
                row["failed_at"] = anchor(started);
            }
            let name = format!("{kind}-{index:03}.json");
            exclusive(&output.join(&name), &row)?;
            receipt["trials"].as_array_mut().unwrap().push(json!({"path":name,"complete":row["complete"],"blake3":blake3::hash(&serde_json::to_vec_pretty(&row)?).to_hex().to_string()}));
            trial?;
        }
        ensure!(
            !fixture.offline_originals.exists(),
            "offline source invariant changed"
        );
        receipt["native_jobs"] = json!(service.scheduler_usage().active);
        receipt["peak_resident_bytes"] = json!(peak_rss());
        receipt["complete"] = json!(true);
        Ok(())
    })();
    if let Err(error) = &result {
        receipt["error"] = json!(format!("{error:#}"));
    }
    receipt["finished"] = anchor(started);
    exclusive(&output.join("receipt.json"), &receipt)?;
    println!("{receipt}");
    result
}
fn verify(folder: &Path) -> Result<()> {
    let receipt: Value =
        serde_json::from_slice(&read_bounded(&folder.join("receipt.json"), 1024 * 1024)?)?;
    ensure!(
        receipt["complete"] == true && receipt["version"] == 1,
        "incomplete run"
    );
    ensure!(
        receipt["source_blake3"]
            == blake3::hash(include_bytes!("preview_navigation_probe.rs"))
                .to_hex()
                .to_string(),
        "probe source mismatch"
    );
    let expected: Vec<(String, u32)> = match receipt["workload"].as_str() {
        Some("warm") => (0..3)
            .map(|i| ("warmup".into(), i))
            .chain((0..100).map(|i| ("warm".into(), i)))
            .chain(std::iter::once(("hot_lru".into(), 0)))
            .collect(),
        Some("fresh") => vec![("fresh".into(), 0)],
        Some("navigation") => (0..10).map(|i| ("navigation".into(), i)).collect(),
        _ => anyhow::bail!("unknown workload"),
    };
    let trials = receipt["trials"].as_array().context("trial list missing")?;
    ensure!(trials.len() == expected.len(), "wrong fixed trial count");
    for (entry, (kind, index)) in trials.iter().zip(expected) {
        let name = format!("{kind}-{index:03}.json");
        ensure!(
            entry["path"] == name && entry["complete"] == true,
            "wrong/failed trial identity"
        );
        let bytes = read_bounded(&folder.join(name), 64 * 1024 * 1024)?;
        ensure!(
            entry["blake3"] == blake3::hash(&bytes).to_hex().to_string(),
            "trial bytes changed"
        );
        let row: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            row["complete"] == true && row["kind"] == kind && row["index"] == index,
            "trial result mismatch"
        );
    }
    println!(
        "{}",
        json!({"complete":true,"trials":trials.len(),"receipt_blake3":blake3::hash(&read_bounded(&folder.join("receipt.json"),1024*1024)?).to_hex().to_string()})
    );
    Ok(())
}
fn main() -> Result<()> {
    match Args::parse().command {
        Command::Overlay { bundle, dataset } => integrated::run(&bundle, &dataset),
        Command::Verify { folder } => verify(&folder),
        Command::Prepare { dataset, output } => prepare(&dataset, &output),
        Command::Run {
            fixture,
            worker,
            output,
            profile,
            workload,
        } => run(&fixture, &worker, &output, profile, workload),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn constrained_profile_keeps_visible_pixels_admitted_beyond_lru() {
        let l = limits(Profile::Constrained);
        assert_eq!(l.requests, VISIBLE);
        assert_eq!(l.decoded_cache_bytes, 32 * 1024 * 1024);
        assert_eq!(l.decoded_live_bytes, 256 * 1024 * 1024);
        assert!(l.per_worker_encoded_bytes <= l.encoded_staging_bytes / 2);
    }
    #[test]
    fn fixed_trace_cancels_fifty_and_keeps_intersection() {
        let a: HashSet<_> = viewport_indices(0).into_iter().collect();
        let b: HashSet<_> = viewport_indices(1).into_iter().collect();
        assert_eq!(a.intersection(&b).count(), 150);
        assert_eq!(a.difference(&b).count(), 50);
        assert_eq!(viewport_indices(99)[0], 4950);
        let wrap = viewport_indices(199);
        assert_eq!(wrap[0], 9950);
        assert_eq!(wrap[199], 149);
        assert_eq!(wrap.iter().collect::<HashSet<_>>().len(), VISIBLE);
    }
}
