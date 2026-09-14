//! Same-session native prepared reuse and validated fallback; synthetic PNG only.
use super::*;
use crate::preview::prepared_cache::{PreparedReference, SourceInstance};
use crate::preview::{EditInputProvenance, PreviewKey, RenderRecord};

#[derive(Clone)]
struct RenderObservation {
    key: PreviewKey,
    prepared: Option<PreparedReference>,
}
fn ready_interactive(
    running: &Running,
    token: &str,
    key: &VariantKey,
    generation: u64,
) -> Result<PreviewStatus> {
    let Response::Preview(mut status) = command(
        &running.bridge,
        Request::Preview {
            catalog: token.into(),
            key: key.clone(),
            tier: PreviewTier::Thumbnail,
            interactive: true,
            viewport: "prepared-route".into(),
            generation: U64(generation),
            foreground: true,
        },
    )?
    else {
        anyhow::bail!("wrong interactive preview reply")
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(status.state, PreviewState::Ready) {
        ensure!(
            matches!(status.state, PreviewState::Queued),
            "interactive preview failed: {:?} {:?}",
            status.state,
            status.message
        );
        ensure!(Instant::now() < deadline, "interactive preview timeout");
        thread::sleep(Duration::from_millis(5));
        let Response::Preview(next) = command(
            &running.bridge,
            Request::PreviewStatus {
                catalog: token.into(),
                ticket: status.ticket,
            },
        )?
        else {
            anyhow::bail!("wrong interactive status reply")
        };
        status = next;
    }
    let bytes = running.bytes(token, &status.ticket)?;
    ensure!(!bytes.bytes().is_empty(), "interactive output is empty");
    drop(bytes);
    Ok(status)
}
fn published_record(root: &Path, key: &PreviewKey) -> Result<RenderRecord> {
    let db = rusqlite::Connection::open_with_flags(
        root.join("application-previews/manifest/previews.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = db.prepare("SELECT r.record FROM render_records r JOIN objects o ON o.key=r.key WHERE r.key=? AND o.status='ready'")?;
    let mut rows = statement.query([key.digest()?])?;
    let row = rows.next()?.context("published render record missing")?;
    let value = row.get_ref(0)?.as_str()?;
    ensure!(value.len() <= 64 * 1024, "fixture render record bound");
    Ok(serde_json::from_str(value)?)
}
fn observation(
    rows: &Mutex<Vec<RenderObservation>>,
    key: &VariantKey,
) -> Result<RenderObservation> {
    let rows = rows.lock().unwrap();
    let mut matching = rows
        .iter()
        .filter(|r| r.key.asset_id == key.asset_id && r.key.variant_id == key.variant_id);
    let row = matching
        .next()
        .context("variant did not use actual Render N")?
        .clone();
    ensure!(
        matching.next().is_none(),
        "variant unexpectedly rendered twice"
    );
    Ok(row)
}

#[test]
#[ignore = "requires explicitly configured built CLI; actual prepared G/C/F/N route"]
fn actual_prepared_reuse_and_source_or_cache_change_fall_back_to_original() -> Result<()> {
    let executable = PathBuf::from(
        std::env::var_os("PHOTOCATALOG_TEST_EXECUTABLE").context("configured CLI required")?,
    );
    ensure!(executable.is_absolute(), "configured CLI must be absolute");
    let (temporary, root, originals, master, checksum) = fixture()?;
    let mut catalog = crate::Catalog::open(&root)?;
    let revision = catalog.edit_variant(&master)?.revision;
    let mut variants = Vec::with_capacity(4);
    for i in 0..4 {
        let variant = catalog.create_edit_variant(&master, revision, &format!("prepared-{i}"))?;
        let edited = catalog.save_edit_recipe(
            &variant.key,
            variant.revision,
            &crate::edit::Recipe::V1(crate::edit::RecipeV1 {
                exposure_ev: (i + 1) as f32 * 0.2,
                ..Default::default()
            }),
        )?;
        variants.push(edited.key);
    }
    drop(catalog);
    let (mut custody, token) = scope(temporary, &executable, &root, &originals, false, false)?;
    let running = custody.running.as_ref().unwrap();
    let rows = Arc::new(Mutex::new(Vec::<RenderObservation>::with_capacity(4)));
    let captured = rows.clone();
    let original_observer = running.parent.observer.lock().unwrap().clone();
    *running.parent.observer.lock().unwrap() = Some(Arc::new(move |call, after| {
        if let Some(original) = &original_observer {
            original(call, after)?;
        }
        if after
            && let Call::Native(r) = call
            && let n::Action::Spawn {
                work: n::Work::Render(work),
                ..
            } = &r.action
        {
            let edit = work
                .edit
                .as_ref()
                .context("interactive render edit missing")?;
            ensure!(edit.interactive, "fixture admitted a noninteractive render");
            let key = work
                .keys
                .iter()
                .find(|key| key.tier == crate::preview::Tier::Thumbnail)
                .context("thumbnail render key missing")?;
            let mut rows = captured.lock().unwrap();
            ensure!(rows.len() < 4, "prepared fixture render observation limit");
            rows.push(RenderObservation {
                key: key.clone(),
                prepared: edit.prepared.clone(),
            });
        }
        Ok(())
    }));

    ready_interactive(running, &token, &variants[0], 1)?;
    let first = observation(&rows, &variants[0])?;
    ensure!(
        first.prepared.is_none(),
        "first render unexpectedly had a prepared candidate"
    );
    ensure!(
        matches!(
            published_record(&root, &first.key)?.edit_input,
            Some(EditInputProvenance::OriginalDecoded)
        ),
        "first render did not decode original"
    );

    ready_interactive(running, &token, &variants[1], 2)?;
    let second = observation(&rows, &variants[1])?;
    let candidate = second
        .prepared
        .as_ref()
        .context("second render did not receive prepared candidate")?;
    let Some(EditInputProvenance::PreparedProxy {
        receipt,
        source_instance_digest,
    }) = published_record(&root, &second.key)?.edit_input
    else {
        anyhow::bail!("second render did not reuse prepared proxy")
    };
    ensure!(
        serde_json::to_value(receipt)? == serde_json::to_value(&candidate.receipt)?,
        "published prepared receipt differs from admitted candidate"
    );
    ensure!(
        source_instance_digest == candidate.source.digest()?,
        "published prepared source differs"
    );

    // Replace only this fixture's synthetic original with identical bytes. The
    // catalog fingerprint remains valid, but the actual source object changes.
    let original_path = originals.join("original.png");
    let source_before = SourceInstance::read(&original_path)?;
    let bytes = std::fs::read(&original_path)?;
    let replacement = originals.join("replacement.png");
    std::fs::write(&replacement, &bytes)?;
    std::fs::rename(&replacement, &original_path)?;
    let source_after = SourceInstance::read(&original_path)?;
    ensure!(
        source_before != source_after,
        "fixture source replacement was not observed"
    );
    ensure!(
        blake3::hash(&std::fs::read(&original_path)?)
            .to_hex()
            .as_str()
            == checksum,
        "fixture replacement changed original bytes"
    );
    ready_interactive(running, &token, &variants[2], 3)?;
    let third = observation(&rows, &variants[2])?;
    let stale = third
        .prepared
        .as_ref()
        .context("source-change render lacked stale candidate")?;
    ensure!(
        stale.source == source_before && stale.source != source_after,
        "source-change case did not admit prior source instance"
    );
    ensure!(
        matches!(
            published_record(&root, &third.key)?.edit_input,
            Some(EditInputProvenance::OriginalDecoded)
        ),
        "changed source instance reused stale prepared data"
    );

    // C replaced the old candidate at the same cache key after original decode.
    // Change one byte without changing its length to isolate content validation.
    let prepared_path = stale.path.to_path()?;
    ensure!(
        prepared_path.starts_with(root.join("application-previews/manifest/prepared")),
        "prepared fixture path escaped catalog cache"
    );
    let mut prepared_bytes = std::fs::read(&prepared_path)?;
    ensure!(!prepared_bytes.is_empty(), "prepared cache file is empty");
    let last = prepared_bytes.len() - 1;
    prepared_bytes[last] ^= 1;
    std::fs::write(&prepared_path, &prepared_bytes)?;
    ready_interactive(running, &token, &variants[3], 4)?;
    let fourth = observation(&rows, &variants[3])?;
    let corrupt = fourth
        .prepared
        .as_ref()
        .context("cache-change render lacked candidate")?;
    ensure!(
        corrupt.source == source_after && corrupt.path.to_path()? == prepared_path,
        "cache-change did not target the current source candidate"
    );
    ensure!(
        corrupt.receipt.bytes == prepared_bytes.len() as u64
            && corrupt.receipt.blake3 != blake3::hash(&prepared_bytes).to_hex().as_str(),
        "cache-change case did not preserve length and alter hash"
    );
    ensure!(
        matches!(
            published_record(&root, &fourth.key)?.edit_input,
            Some(EditInputProvenance::OriginalDecoded)
        ),
        "changed cache bytes did not fall back to original"
    );
    ensure!(
        rows.lock().unwrap().len() == 4,
        "prepared route did not render all four variants"
    );
    ensure!(
        blake3::hash(&std::fs::read(&original_path)?)
            .to_hex()
            .as_str()
            == checksum,
        "prepared route changed original content"
    );

    // Keep Custody and TempDir reachable until every normal retirement succeeds.
    command(&running.bridge, Request::Close { catalog: token })?;
    running.bridge.try_shutdown()?;
    running.parent.finish_after_dependents(false)?;
    let owner = running.parent.native_owner()?;
    for row in running.observed.lock().unwrap().iter() {
        ensure!(
            owner.status(&row.root, row.operation).is_err(),
            "prepared native owner retained after close"
        );
    }
    {
        let state = running.bridge.0.shared.state.lock().unwrap();
        ensure!(
            state.reaped && state.child_finished && state.filesystem_verified,
            "prepared route retirement unverified"
        );
    }
    eprintln!(
        "prepared route verified four Render N variants, reuse and two fallbacks; C={} F={} checked retired",
        running.bridge.status().pid,
        running.client.pid()
    );
    custody.running.take();
    // Begin a fresh small-budget C owner so none of the four decoded RGB cache
    // entries survive. Existing encoded records remain the exact same keys.
    drop(prepared_bytes);
    drop(bytes);
    let temporary = custody
        .temporary
        .take()
        .context("prepared fixture directory missing")?;
    let (mut cold, token) = scope(temporary, &executable, &root, &originals, true, false)?;
    let running = cold.running.as_ref().unwrap();
    let begin = Instant::now();
    for (index, variant) in variants.iter().enumerate() {
        ready_interactive(running, &token, variant, 10 + index as u64)?;
    }
    let elapsed = begin.elapsed();
    {
        let observed = running.observed.lock().unwrap();
        ensure!(
            observed.len() == 4 && observed.iter().all(|row| !row.render),
            "cold prepared-variant batch must use four DecodeEncoded N and no Render"
        );
    }
    // Report a small observed batch, without turning it into a 200-preview page
    // or whole-application RSS claim. Each helper above delivered nonempty bytes.
    eprintln!(
        "FS8 cold encoded-cache batch: count=4 working_bytes=2097152 elapsed_ms={:.3} ms_per_preview={:.3}",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 250.0
    );
    command(&running.bridge, Request::Close { catalog: token })?;
    running.bridge.try_shutdown()?;
    running.parent.finish_after_dependents(false)?;
    let owner = running.parent.native_owner()?;
    for row in running.observed.lock().unwrap().iter() {
        ensure!(
            owner.status(&row.root, row.operation).is_err(),
            "cold batch native owner retained after close"
        );
    }
    {
        let state = running.bridge.0.shared.state.lock().unwrap();
        ensure!(
            state.reaped && state.child_finished && state.filesystem_verified,
            "cold batch retirement unverified"
        );
    }
    eprintln!(
        "cold batch verified four DecodeEncoded N and bytes; C={} F={} checked retired",
        running.bridge.status().pid,
        running.client.pid()
    );
    cold.running.take();
    Ok(())
}
