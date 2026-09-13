use super::*;
use crate::{
    application::{self as app, Bridge, Config, Reply},
    edit::{NoiseReduction, NormalizedRect, RecipeV1, Sharpening, WhiteBalance},
    storage_volume::NativePath,
};
use anyhow::{Result, ensure};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

fn fixture() -> Result<(tempfile::TempDir, Catalog, VariantKey)> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path().join("catalog"))?;
    let path = NativePath::from_path(&temp.path().join("missing.png"));
    let bytes = match &path {
        NativePath::UnixBytes(v) => v.clone(),
        NativePath::WindowsWide(v) => v.iter().flat_map(|u| u.to_le_bytes()).collect(),
    };
    c.db.execute("INSERT INTO assets(id,location,path_display,state) VALUES('photo',?1,'/offline/名前.png','pending')",[bytes])?;
    c.record_storage_path(
        "photo",
        &NativePath::from_path(&temp.path().join("missing.png")),
    )?;
    Ok((temp, c, VariantKey::master("photo")))
}
fn job(r: Response) -> Job {
    let Response::Job(j) = r else { panic!("job") };
    j
}
fn run(c: &mut Catalog, ctl: &Arc<Mutex<Control>>, j: &str) -> Operation {
    let Response::Operation(Some(op)) = execute(
        c,
        Request::Run { job: j.into() },
        &Limits::default(),
        ctl,
        false,
    )
    .unwrap() else {
        panic!("operation")
    };
    op
}
fn queue(c: &mut Catalog, source: &VariantKey, targets: &[VariantKey]) -> Result<String> {
    let rev = c.edit_variant(source)?.revision;
    let j = c.begin_edit_copy(source, rev, &[AdjustmentGroup::Exposure])?;
    c.append_edit_copy(
        &j.id,
        0,
        &targets
            .iter()
            .map(|key| core::EditTarget {
                key: key.clone(),
                expected_revision: c.edit_variant(key).unwrap().revision,
            })
            .collect::<Vec<_>>(),
    )?;
    c.seal_edit_copy(&j.id, targets.len() as i64)?;
    Ok(j.id)
}
#[test]
fn all_groups_are_frozen_and_preserve_other_target_settings() -> Result<()> {
    let (_temp, mut c, source) = fixture()?;
    let groups = [
        AdjustmentGroup::Geometry,
        AdjustmentGroup::Exposure,
        AdjustmentGroup::WhiteBalance,
        AdjustmentGroup::Tone,
        AdjustmentGroup::Color,
        AdjustmentGroup::Sharpening,
        AdjustmentGroup::NoiseReduction,
    ];
    let settings = RecipeV1 {
        crop: Some(NormalizedRect {
            left: 0.1,
            top: 0.1,
            right: 0.9,
            bottom: 0.8,
        }),
        straighten_degrees: 2.,
        exposure_ev: 1.5,
        white_balance: WhiteBalance::TemperatureTint {
            kelvin: 5700,
            tint: 2.,
        },
        contrast: 0.2,
        highlights: -0.2,
        shadows: 0.3,
        saturation: 0.8,
        vibrance: 0.4,
        sharpening: Sharpening {
            amount: 0.3,
            radius_px: 1.,
        },
        noise_reduction: NoiseReduction {
            luminance: 0.2,
            chroma: 0.4,
        },
    };
    c.save_edit_recipe(&source, 0, &Recipe::V1(settings.clone()))?;
    let mut jobs = Vec::new();
    for (n, group) in groups.iter().enumerate() {
        let target = c
            .create_edit_variant(&source, 1, &format!("Target {n}"))?
            .key;
        c.save_edit_recipe(&target, 0, &Recipe::default())?;
        let j = c.begin_edit_copy(&source, 1, &[*group])?;
        c.append_edit_copy(
            &j.id,
            0,
            &[core::EditTarget {
                key: target.clone(),
                expected_revision: 1,
            }],
        )?;
        c.seal_edit_copy(&j.id, 1)?;
        jobs.push((j.id, target, *group));
    }
    let all = c.create_edit_variant(&source, 1, "All groups")?.key;
    c.save_edit_recipe(&all, 0, &Recipe::default())?;
    let alljob = c.begin_edit_copy(&source, 1, &groups)?;
    c.append_edit_copy(
        &alljob.id,
        0,
        &[core::EditTarget {
            key: all.clone(),
            expected_revision: 1,
        }],
    )?;
    c.seal_edit_copy(&alljob.id, 1)?;
    c.save_edit_recipe(&source, 1, &Recipe::default())?;
    for (id, target, group) in jobs {
        let frozen = c.edit_copy_description(&id)?;
        assert_eq!(frozen.source.expected_revision, 1);
        assert_eq!(frozen.recipe, Recipe::V1(settings.clone()));
        c.apply_edit_copy_step(&id, 1)?;
        let mut expected = RecipeV1::default();
        match group {
            AdjustmentGroup::Geometry => {
                expected.crop = settings.crop;
                expected.straighten_degrees = settings.straighten_degrees
            }
            AdjustmentGroup::Exposure => expected.exposure_ev = settings.exposure_ev,
            AdjustmentGroup::WhiteBalance => {
                expected.white_balance = settings.white_balance.clone()
            }
            AdjustmentGroup::Tone => {
                expected.contrast = settings.contrast;
                expected.highlights = settings.highlights;
                expected.shadows = settings.shadows
            }
            AdjustmentGroup::Color => {
                expected.saturation = settings.saturation;
                expected.vibrance = settings.vibrance
            }
            AdjustmentGroup::Sharpening => expected.sharpening = settings.sharpening,
            AdjustmentGroup::NoiseReduction => expected.noise_reduction = settings.noise_reduction,
        }
        assert_eq!(c.edit_variant(&target)?.recipe, Recipe::V1(expected));
        let changes: i64 = c.db.query_row(
            "SELECT COUNT(*) FROM edit_changes WHERE asset_id=?1 AND variant_id=?2 AND kind='copy'",
            rusqlite::params![target.asset_id, target.variant_id],
            |r| r.get(0),
        )?;
        assert_eq!(changes, 1);
    }
    c.apply_edit_copy_step(&alljob.id, 1)?;
    assert_eq!(c.edit_variant(&all)?.recipe, Recipe::V1(settings));
    assert_eq!(c.edit_variant(&source)?.recipe, Recipe::default());
    Ok(())
}
#[test]
fn pages_high_ids_names_byte_admission_and_sparse_pending_seek() -> Result<()> {
    let (_temp, mut c, source) = fixture()?;
    let target = c
        .create_edit_variant(&source, 0, "Exact selected copy")?
        .key;
    let id = queue(&mut c, &source, std::slice::from_ref(&target))?;
    let high = 9_007_199_254_740_999i64;
    c.db.execute(
        "UPDATE edit_copy_jobs SET sequence=?1 WHERE id=?2",
        rusqlite::params![high, id],
    )?;
    c.db.execute(
        "UPDATE edit_copy_items SET sequence=?1 WHERE job=?2",
        rusqlite::params![high, id],
    )?;
    let r = read(
        &c,
        Request::Jobs {
            after: I64(0),
            limit: U64(1),
        },
        &Limits::default(),
    )?;
    assert_eq!(
        serde_json::to_value(&r)?["value"]["rows"][0]["sequence"],
        high.to_string()
    );
    let r = read(
        &c,
        Request::Items {
            job: id.clone(),
            after: I64(0),
            limit: U64(1),
        },
        &Limits::default(),
    )?;
    let Response::Items { rows, next } = r else {
        panic!("items")
    };
    assert!(next.is_none());
    assert_eq!(rows[0].sequence, I64(high));
    assert_eq!(rows[0].name.filename, "名前.png");
    assert_eq!(rows[0].name.variant_label, "Exact selected copy");
    assert!(
        serde_json::from_value::<Request>(
            serde_json::json!({"command":"items","args":{"job":id,"after":high,"limit":"1"}})
        )
        .is_err()
    );
    let tx = c.db.transaction()?;
    for n in 1..=10_000 {
        tx.execute("INSERT INTO edit_copy_items(job,sequence,asset_id,variant_id,expected_revision,state) VALUES(?1,?2,'photo',?3,0,'applied')",rusqlite::params![id,n,format!("old-{n}")])?;
    }
    tx.execute(
        "UPDATE edit_copy_jobs SET total=10001,completed=10000 WHERE id=?1",
        [&id],
    )?;
    tx.commit()?;
    let work = Arc::new(AtomicUsize::new(0));
    let counter = work.clone();
    c.db.progress_handler(
        1,
        Some(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )?;
    let done = c.apply_edit_copy_step(&id, 1)?;
    c.db.progress_handler(0, None::<fn() -> bool>)?;
    assert_eq!(done.completed, 10001);
    assert!(
        work.load(Ordering::Relaxed) < 10_000,
        "pending seek scanned prefix: {}",
        work.load(Ordering::Relaxed)
    );
    let details:Vec<String>=c.db.prepare("EXPLAIN QUERY PLAN SELECT sequence FROM edit_copy_items WHERE job=?1 AND state='pending' ORDER BY sequence LIMIT 1")?.query_map([&id],|r|r.get(3))?.collect::<rusqlite::Result<_>>()?;
    assert!(details.iter().any(|s| s.contains("edit_copy_pending")));
    assert!(!details.iter().any(|s| s.contains("TEMP B-TREE")));
    let narrow = Limits {
        page_rows: 10,
        page_bytes: 600,
        ..Limits::default()
    };
    let mut after = I64(9997);
    let mut seen = Vec::new();
    loop {
        let Response::Items { rows, next } = read(
            &c,
            Request::Items {
                job: id.clone(),
                after,
                limit: U64(10),
            },
            &narrow,
        )?
        else {
            panic!("items")
        };
        seen.extend(rows.into_iter().map(|r| r.sequence.0));
        let Some(next) = next else { break };
        assert!(next > after);
        after = next;
    }
    assert_eq!(seen, [9998, 9999, 10000, high]);
    let small = Limits {
        page_bytes: 8,
        ..Limits::default()
    };
    assert!(
        read(
            &c,
            Request::Items {
                job: id.clone(),
                after: I64(10000),
                limit: U64(1)
            },
            &small
        )
        .is_err()
    );
    c.db.execute(
        "UPDATE edit_copy_jobs SET groups_json=?1 WHERE id=?2",
        rusqlite::params!["界".repeat(400), id],
    )?;
    assert!(
        c.edit_copy_description(&id)
            .unwrap_err()
            .to_string()
            .contains("groups exceed")
    );
    c.db.execute(
        "UPDATE edit_copy_jobs SET recipe=zeroblob(65537) WHERE id=?1",
        [&id],
    )?;
    assert!(c.edit_copy_description(&id).is_err());
    Ok(())
}
#[test]
fn run_foreground_holds_cas_cancel_and_explicit_resume() -> Result<()> {
    let (_temp, mut c, source) = fixture()?;
    let a = c.create_edit_variant(&source, 0, "A")?.key;
    let b = c.create_edit_variant(&source, 0, "B")?.key;
    let ctl = Arc::new(Mutex::new(Control::default()));
    let id = job(execute(
        &mut c,
        Request::Begin {
            source: source.clone(),
            expected_revision: I64(0),
            groups: vec![AdjustmentGroup::Exposure],
        },
        &Limits::default(),
        &ctl,
        false,
    )?)
    .id;
    execute(
        &mut c,
        Request::Append {
            job: id.clone(),
            expected_total: I64(0),
            targets: vec![Target {
                key: a.clone(),
                expected_revision: I64(0),
            }],
        },
        &Limits::default(),
        &ctl,
        false,
    )?;
    assert!(
        execute(
            &mut c,
            Request::Append {
                job: id.clone(),
                expected_total: I64(0),
                targets: vec![Target {
                    key: b.clone(),
                    expected_revision: I64(0)
                }]
            },
            &Limits::default(),
            &ctl,
            false
        )
        .is_err()
    );
    execute(
        &mut c,
        Request::Append {
            job: id.clone(),
            expected_total: I64(1),
            targets: vec![Target {
                key: b.clone(),
                expected_revision: I64(0),
            }],
        },
        &Limits::default(),
        &ctl,
        false,
    )?;
    execute(
        &mut c,
        Request::Seal {
            job: id.clone(),
            expected_total: I64(2),
        },
        &Limits::default(),
        &ctl,
        false,
    )?;
    c.save_edit_recipe(
        &a,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 2.,
            ..Default::default()
        }),
    )?;
    let ctl = Arc::new(Mutex::new(Control::default()));
    assert!(
        execute(
            &mut c,
            Request::Run { job: id.clone() },
            &Limits::default(),
            &ctl,
            true
        )
        .is_err()
    );
    let op = run(&mut c, &ctl, &id);
    assert!(!advance(&mut c, &ctl, true, false, None));
    assert_eq!(c.edit_copy_job(&id)?.completed, 0);
    assert!(!advance(&mut c, &ctl, false, true, None));
    assert_eq!(
        ctl.lock().unwrap().status(None)?.unwrap().phase,
        Phase::Paused
    );
    assert!(advance(&mut c, &ctl, false, false, None));
    assert_eq!(c.edit_copy_job(&id)?.completed, 1);
    assert_eq!(c.edit_copy_items(&id, 0, 1)?[0].state, "conflict");
    close(&mut c, &ctl);
    assert!(!advance(&mut c, &ctl, false, false, None));
    assert_eq!(c.edit_copy_job(&id)?.state, "queued");
    let resumed = run(&mut c, &ctl, &id);
    assert_ne!(op.id, resumed.id);
    assert!(ctl.lock().unwrap().status(Some(&op.id)).is_err());
    ctl.lock().unwrap().cancel(&id, Some(&resumed.id))?;
    assert!(!advance(&mut c, &ctl, false, true, None));
    assert_eq!(c.edit_copy_job(&id)?.state, "queued");
    assert!(advance(&mut c, &ctl, true, false, None));
    assert_eq!(c.edit_copy_job(&id)?.state, "canceled");
    assert_eq!(c.edit_variant(&b)?.revision, 0);
    let r = execute(
        &mut c,
        Request::Cancel {
            job: id.clone(),
            operation: None,
        },
        &Limits::default(),
        &ctl,
        false,
    )?;
    assert!(matches!(r, Response::Job(_)));
    let id2 = queue(&mut c, &source, std::slice::from_ref(&b))?;
    run(&mut c, &ctl, &id2);
    assert!(advance(&mut c, &ctl, false, false, None));
    assert_eq!(
        ctl.lock().unwrap().status(None)?.unwrap().phase,
        Phase::Complete
    );
    Ok(())
}
fn call(bridge: &Bridge, r: app::Request) -> Result<app::Response> {
    match bridge.submit(r)?.recv() {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(error.into()),
    }
}
fn invoke(bridge: &Bridge, token: &str, r: Request) -> Result<Response> {
    let app::Response::EditCopy(r) = call(
        bridge,
        app::Request::EditCopy {
            catalog: token.into(),
            request: Box::new(r),
        },
    )?
    else {
        anyhow::bail!("copy reply")
    };
    Ok(*r)
}
#[test]
fn actor_cancel_bypasses_queue_and_close_reopen_never_autoruns() -> Result<()> {
    let (temp, mut c, source) = fixture()?;
    let targets = (0..3)
        .map(|n| {
            c.create_edit_variant(&source, 0, &format!("T{n}"))
                .map(|v| v.key)
        })
        .collect::<Result<Vec<_>>>()?;
    let id = queue(&mut c, &source, &targets)?;
    drop(c);
    let held = Arc::new(AtomicBool::new(true));
    let h = held.clone();
    let (entered, events) = mpsc::channel();
    let checkpoint = Arc::new(move |stage: &str, cancel: &AtomicBool| {
        if stage == "copy_before_step" {
            let _ = entered.send(());
            while h.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        original_roots: vec![temp.path().join("originals")],
        cache_root: None,
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Limits::default(),
        import_checkpoint: Some(checkpoint),
    })?;
    let root = temp.path().join("catalog");
    let app::Response::Status(s) = call(
        &bridge,
        app::Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        panic!("open")
    };
    let token = s.catalog.unwrap();
    let Response::Operation(None) = invoke(&bridge, &token, Request::Status { operation: None })?
    else {
        panic!("no autorun")
    };
    let Response::Operation(Some(op)) = invoke(&bridge, &token, Request::Run { job: id.clone() })?
    else {
        panic!("run")
    };
    events.recv_timeout(Duration::from_secs(5))?;
    let Response::Operation(Some(s)) = invoke(
        &bridge,
        &token,
        Request::Status {
            operation: Some(op.id.clone()),
        },
    )?
    else {
        panic!("status")
    };
    assert_eq!(s.job.completed, I64(0));
    assert!(
        invoke(
            &bridge,
            "stale",
            Request::Cancel {
                job: id.clone(),
                operation: Some(op.id.clone())
            }
        )
        .is_err()
    );
    assert!(
        invoke(
            &bridge,
            &token,
            Request::Cancel {
                job: id.clone(),
                operation: Some("stale".into())
            }
        )
        .is_err()
    );
    let Response::Operation(Some(cancel)) = invoke(
        &bridge,
        &token,
        Request::Cancel {
            job: id.clone(),
            operation: Some(op.id.clone()),
        },
    )?
    else {
        panic!("cancel")
    };
    assert_eq!(cancel.phase, Phase::CancelRequested);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let Response::Operation(Some(s)) = invoke(
            &bridge,
            &token,
            Request::Status {
                operation: Some(op.id.clone()),
            },
        )?
        else {
            panic!("status")
        };
        if s.phase == Phase::Canceled {
            break;
        }
        ensure!(Instant::now() < until, "cancel deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
    held.store(false, Ordering::Release);
    call(&bridge, app::Request::Close { catalog: token })?;
    let app::Response::Status(s) = call(
        &bridge,
        app::Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        panic!("reopen")
    };
    let token = s.catalog.unwrap();
    assert!(matches!(
        invoke(&bridge, &token, Request::Status { operation: None })?,
        Response::Operation(None)
    ));
    assert_eq!(
        job(invoke(&bridge, &token, Request::Job { job: id })?).state,
        "canceled"
    );
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(root)?;
    for key in targets {
        assert_eq!(c.edit_variant(&key)?.revision, 0)
    }
    Ok(())
}

#[test]
fn foreground_preview_bytes_precede_copy_but_background_bytes_do_not() {
    for foreground in [false, true] {
        let (reply, _) = mpsc::sync_channel(1);
        let e = app::Envelope {
            work: app::Work::Bytes {
                catalog: "catalog".into(),
                ticket: "ticket".into(),
                foreground,
                reply,
            },
            cancel: Cancellation::default(),
            created: Instant::now(),
        };
        assert_eq!(e.priority(), if foreground { 3 } else { 4 });
        assert_eq!(e.before_copy(), foreground);
    }
}

#[test]
fn actor_defers_copy_cancel_during_relink_then_persists_it_after_close_drain() -> Result<()> {
    let (temp, mut c, source) = fixture()?;
    let targets = (0..4)
        .map(|n| {
            c.create_edit_variant(&source, 0, &format!("Held target {n}"))
                .map(|v| v.key)
        })
        .collect::<Result<Vec<_>>>()?;
    let job = queue(&mut c, &source, &targets)?;
    let plan = c.begin_relink_review(crate::catalog_storage::RelinkScope::Asset {
        asset_id: source.asset_id.clone(),
        destinations: vec![NativePath::from_path(
            &temp.path().join("relink-destination"),
        )],
    })?;
    drop(c);
    let release = Arc::new(AtomicBool::new(false));
    let copied = release.clone();
    let (entered, events) = mpsc::channel();
    let checkpoint = Arc::new(move |stage: &str, cancel: &AtomicBool| {
        if stage == "copy_before_step" || stage == "relink_commit" {
            let _ = entered.send(stage.to_owned());
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline
                && !cancel.load(Ordering::Acquire)
                && (stage == "relink_commit" || !copied.load(Ordering::Acquire))
            {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        original_roots: vec![temp.path().join("originals")],
        cache_root: None,
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Limits::default(),
        import_checkpoint: Some(checkpoint),
    })?;
    let root = temp.path().join("catalog");
    let app::Response::Status(s) = call(
        &bridge,
        app::Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        panic!("open")
    };
    let token = s.catalog.unwrap();
    let Response::Operation(Some(op)) = invoke(&bridge, &token, Request::Run { job: job.clone() })?
    else {
        panic!("run")
    };
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5))?,
        "copy_before_step"
    );
    let pending = bridge.submit(app::Request::Relink {
        catalog: token.clone(),
        request: Box::new(app::relink::Request::Revise {
            plan: plan.id,
            revision: I64(plan.revision),
            changes: vec![],
        }),
    })?;
    release.store(true, Ordering::Release);
    assert!(matches!(
        pending.recv(),
        Reply::Ok {
            value: app::Response::Relink(_)
        }
    ));
    // Ignore an already emitted step checkpoint; the actor admits foreground relink first.
    loop {
        if events.recv_timeout(Duration::from_secs(5))? == "relink_commit" {
            break;
        }
    }
    let Response::Operation(Some(s)) = invoke(
        &bridge,
        &token,
        Request::Status {
            operation: Some(op.id.clone()),
        },
    )?
    else {
        panic!("status")
    };
    assert_eq!(s.phase, Phase::Paused);
    assert!(s.job.completed.0 < 4);
    let done = s.job.completed;
    assert!(
        invoke(
            &bridge,
            &token,
            Request::Begin {
                source: source.clone(),
                expected_revision: I64(0),
                groups: vec![AdjustmentGroup::Exposure]
            }
        )
        .is_err()
    );
    let Response::Operation(Some(s)) = invoke(
        &bridge,
        &token,
        Request::Cancel {
            job: job.clone(),
            operation: Some(op.id),
        },
    )?
    else {
        panic!("cancel")
    };
    assert_eq!(s.phase, Phase::CancelRequested);
    let Response::Job(saved) = invoke(&bridge, &token, Request::Job { job: job.clone() })? else {
        panic!("job")
    };
    assert_eq!(saved.state, "queued");
    assert_eq!(saved.completed, done);
    call(&bridge, app::Request::Close { catalog: token })?;
    let app::Response::Status(s) = call(
        &bridge,
        app::Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        panic!("reopen")
    };
    let token = s.catalog.unwrap();
    let Response::Job(saved) = invoke(&bridge, &token, Request::Job { job })? else {
        panic!("job")
    };
    assert_eq!(saved.state, "canceled");
    assert_eq!(saved.completed, done);
    assert!(matches!(
        invoke(&bridge, &token, Request::Status { operation: None })?,
        Response::Operation(None)
    ));
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(root)?;
    for key in targets.into_iter().skip(done.0 as usize) {
        assert_eq!(c.edit_variant(&key)?.revision, 0)
    }
    Ok(())
}
