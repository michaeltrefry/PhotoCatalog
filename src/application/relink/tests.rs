use super::*;
use crate::application::{self as app, Bridge, Config, Reply};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

fn call(bridge: &Bridge, request: app::Request) -> Result<app::Response> {
    let context = format!("relink test waiting for {request:?}");
    match bridge
        .submit(request)?
        .receiver
        .recv_timeout(Duration::from_secs(5))
        .with_context(|| context)?
    {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(error.into()),
    }
}
fn relink(bridge: &Bridge, token: &str, request: Request) -> Result<Response> {
    let app::Response::Relink(value) = call(
        bridge,
        app::Request::Relink {
            catalog: token.into(),
            request: Box::new(request),
        },
    )?
    else {
        anyhow::bail!("wrong relink reply")
    };
    Ok(*value)
}
fn operation(value: Response) -> Operation {
    let Response::Operation(Some(value)) = value else {
        panic!("operation reply")
    };
    value
}
fn terminal(bridge: &Bridge, token: &str, id: &str) -> Result<Operation> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let value = operation(relink(
            bridge,
            token,
            Request::Status {
                operation: Some(id.into()),
            },
        )?);
        if matches!(
            value.phase,
            Phase::Complete | Phase::Canceled | Phase::Failed
        ) {
            return Ok(value);
        }
        ensure!(Instant::now() < until, "operation deadline: {value:?}");
        thread::sleep(Duration::from_millis(2));
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    originals: PathBuf,
    destination: PathBuf,
    key: VariantKey,
    bytes: Vec<u8>,
}
fn fixture() -> Result<Fixture> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("seed.png");
    image::RgbImage::from_pixel(16, 12, image::Rgb([10u8, 40, 80])).save(&path)?;
    let bytes = std::fs::read(&path)?;
    let destination = temp.path().join("moved.png");
    std::fs::copy(&path, &destination)?;
    let root = temp.path().join("catalog");
    let mut catalog = Catalog::open(&root)?;
    catalog.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(catalog.browse(0, 1)?[0].id.clone());
    drop(catalog);
    Ok(Fixture {
        _temp: temp,
        root,
        originals,
        destination,
        key,
        bytes,
    })
}
fn open(
    f: &Fixture,
    checkpoint: Option<crate::import_preparation::Checkpoint>,
) -> Result<(Bridge, String)> {
    let bridge = Bridge::spawn(Config {
        worker_executable: std::env::current_exe()?,
        cache_root: None,
        original_roots: vec![f.originals.clone()],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Limits::default(),
        import_checkpoint: checkpoint,
    })?;
    let app::Response::Status(s) = call(
        &bridge,
        app::Request::OpenExisting {
            path: NativePath::from_path(&f.root),
        },
    )?
    else {
        anyhow::bail!("open")
    };
    Ok((bridge, s.catalog.context("catalog token")?))
}
fn begin(f: &Fixture, bridge: &Bridge, token: &str) -> Result<Plan> {
    let Response::Plan(p) = relink(
        bridge,
        token,
        Request::Begin {
            scope: Scope::Asset {
                asset_id: f.key.asset_id.clone(),
                destinations: vec![NativePath::from_path(&f.destination)],
            },
        },
    )?
    else {
        anyhow::bail!("begin")
    };
    Ok(p)
}
fn ready(f: &Fixture) -> Result<Plan> {
    let mut c = Catalog::open(&f.root)?;
    let mut p = c.begin_relink_review(core::RelinkScope::Asset {
        asset_id: f.key.asset_id.clone(),
        destinations: vec![NativePath::from_path(&f.destination)],
    })?;
    while p.state == "preparing" {
        p = c.prepare_relink_batch(&p.id, 1)?;
    }
    ensure!(p.state == "ready", "ready: {p:?}");
    Ok(p.into())
}

#[test]
fn exact_wire_and_backend_mount_authority() -> Result<()> {
    let request = Request::Sources {
        plan: "plan".into(),
        revision: I64(i64::MAX),
        sequence: I64(i64::MAX),
        after: I64(0),
        limit: U64(100),
    };
    let value = serde_json::to_value(&request)?;
    assert_eq!(value["args"]["revision"], i64::MAX.to_string());
    assert!(serde_json::from_value::<Request>(serde_json::json!({"command":"prepare","args":{"plan":"p","revision":1,"batch_rows":"1"}})).is_err());
    let f = fixture()?;
    let mut catalog = Catalog::open(&f.root)?;
    let mut coordinator = Coordinator::default();
    let control = Arc::new(Mutex::new(Control::default()));
    let bad = coordinator
        .execute(
            &mut catalog,
            Request::Begin {
                scope: Scope::Volume {
                    logical_volume: "known".into(),
                    mount_token: "fabricated".into(),
                },
            },
            &Limits::default(),
            &control,
        )
        .unwrap_err();
    assert!(matches!(bad.code, ErrorCode::StaleSession));
    assert!(catalog.relink_plans("", 1)?.is_empty());
    let native = NativePath::UnixBytes(vec![47, 255, 128]);
    let scope = Scope::Prefix {
        from: core::PathReference::LegacyWindows(vec![67, 58, 92, 0xd800]),
        destinations: vec![native.clone()],
    };
    let Scope::Prefix { destinations, .. } = serde_json::from_slice(&serde_json::to_vec(&scope)?)?
    else {
        panic!()
    };
    assert_eq!(destinations, [native]);
    Ok(())
}

#[test]
fn review_pages_bind_revision_and_do_not_skip_byte_rejected_rows() -> Result<()> {
    let f = fixture()?;
    for (name, value) in [("second.png", 90u8), ("third.png", 130u8)] {
        image::RgbImage::from_pixel(16, 12, image::Rgb([value, 40, 80]))
            .save(f.originals.join(name))?;
    }
    let mut c = Catalog::open(&f.root)?;
    c.import(&f.originals, None, |_| Ok(()))?;
    let mut p = c.begin_relink_review(core::RelinkScope::Prefix {
        from: core::PathReference::native(&f.originals.canonicalize()?),
        destinations: vec![NativePath::from_path(&f.originals.canonicalize()?)],
    })?;
    while p.state == "preparing" {
        p = c.prepare_relink_batch(&p.id, 1)?;
    }
    let expected = c.relink_items(&p.id, 0, 100)?;
    assert_eq!(expected.len(), 3);
    let max_row = expected
        .iter()
        .map(|p| {
            serde_json::to_vec(&vec![Item::from(p.clone())])
                .unwrap()
                .len()
        })
        .max()
        .unwrap();
    let limits = Limits {
        page_bytes: max_row + 16,
        ..Limits::default()
    };
    let control = Arc::new(Mutex::new(Control::default()));
    let mut coordinator = Coordinator::default();
    let mut after = I64(0);
    let mut observed = Vec::new();
    loop {
        let Response::Items { rows, next } = coordinator.execute(
            &mut c,
            Request::Items {
                plan: p.id.clone(),
                revision: I64(p.revision),
                after,
                limit: U64(3),
            },
            &limits,
            &control,
        )?
        else {
            panic!("items")
        };
        assert_eq!(rows.len(), 1);
        observed.push(rows[0].sequence.0);
        let Some(next) = next else { break };
        after = next;
    }
    assert_eq!(
        observed,
        expected.iter().map(|p| p.sequence).collect::<Vec<_>>()
    );
    let stale = coordinator
        .execute(
            &mut c,
            Request::Items {
                plan: p.id.clone(),
                revision: I64(p.revision + 1),
                after: I64(0),
                limit: U64(1),
            },
            &limits,
            &control,
        )
        .unwrap_err();
    assert!(matches!(stale.code, ErrorCode::StaleSession));
    let tiny = Limits {
        page_bytes: 1,
        ..limits
    };
    assert!(matches!(
        coordinator
            .execute(
                &mut c,
                Request::Items {
                    plan: p.id,
                    revision: I64(p.revision),
                    after: I64(0),
                    limit: U64(1)
                },
                &tiny,
                &control,
            )
            .unwrap_err()
            .code,
        ErrorCode::ResourceLimit
    ));
    Ok(())
}

#[test]
fn legacy_asset_exclusions_charge_sparse_indexed_scan_work() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut c = Catalog::open(temp.path())?;
    let p = c.begin_relink_review(core::RelinkScope::Prefix {
        from: core::PathReference::native(&temp.path().join("old")),
        destinations: vec![NativePath::from_path(&temp.path().join("new"))],
    })?;
    let tx = c.db.transaction()?;
    for n in 1..=10_000 {
        let id = format!("sparse-{n}");
        tx.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
            rusqlite::params![id, format!("/synthetic/{n}").as_bytes()],
        )?;
        tx.execute("INSERT INTO storage_items(plan,sequence,asset_id,status,detail,data) VALUES(?1,?2,?3,'matched','fixture','{}')",rusqlite::params![p.id,n,id])?;
    }
    tx.commit()?;
    let limits = Limits {
        scan_rows: 3,
        ..Limits::default()
    };
    let measured = |position| -> Result<(Vec<RuleRow>, Option<RuleCursor>)> {
        let work = Arc::new(AtomicUsize::new(0));
        let counter = work.clone();
        c.db.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )?;
        let result = rules(
            &c,
            &p.id,
            I64(p.revision),
            Some(RuleCursor {
                plan: p.id.clone(),
                revision: I64(p.revision),
                stage: RuleStage::ExcludedAsset,
                position: I64(position),
                source: I64(0),
                entity: String::new(),
            }),
            U64(10),
            &limits,
        );
        c.db.progress_handler(0, None::<fn() -> bool>)?;
        let Response::Rules {
            rows,
            next,
            scanned,
        } = result?
        else {
            panic!("rules")
        };
        assert!(scanned.0 <= 3);
        assert!(
            work.load(Ordering::Relaxed) < 2000,
            "sparse rule page performed {} VM steps",
            work.load(Ordering::Relaxed)
        );
        Ok((rows, next))
    };
    // No exclusions still yields exact scan progress, without scanning the plan.
    let (rows, next) = measured(0)?;
    assert!(rows.is_empty());
    assert_eq!(next.unwrap().position, I64(3));
    let (rows, next) = measured(9_997)?;
    assert!(rows.is_empty());
    assert_eq!(next.unwrap().position, I64(10_000));
    assert!(measured(10_000)?.1.is_none());
    // An exclusion at the end cannot make an initial page jump across candidates.
    c.db.execute(
        "UPDATE storage_items SET status='excluded' WHERE plan=?1 AND sequence=10000",
        [&p.id],
    )?;
    let (rows, next) = measured(0)?;
    assert!(rows.is_empty());
    assert_eq!(next.unwrap().position, I64(3));
    let (rows, next) = measured(9_997)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label.as_deref(), Some("sparse-10000"));
    assert!(
        matches!(&rows[0].rule,Rule::Override(Override::Asset{asset_id,candidates}) if asset_id=="sparse-10000" && candidates.is_empty())
    );
    assert_eq!(next.unwrap().position, I64(10_000));
    assert!(measured(10_000)?.1.is_none());
    Ok(())
}

#[test]
fn saved_rules_include_legacy_exclusions_with_bounded_empty_continuations() -> Result<()> {
    let f = fixture()?;
    let p = ready(&f)?;
    let mut c = Catalog::open(&f.root)?;
    let item = c.relink_items(&p.id, 0, 1)?.pop().unwrap();
    let mut last = 0;
    for n in 0..13 {
        c.db.execute("INSERT INTO metadata_sources(asset_id,kind,locator,display,association,availability) VALUES(?1,'sidecar',?2,?3,'associated','available')",rusqlite::params![f.key.asset_id,format!("/synthetic/{n}.xmp").into_bytes(),format!("sidecar {n}")])?;
        last = c.db.last_insert_rowid();
        c.db.execute("INSERT INTO storage_source_items(plan,sequence,source_id,status,detail,data) VALUES(?1,?2,?3,'matched','fixture','{}')",rusqlite::params![p.id,item.sequence,last])?;
    }
    c.exclude_relink_source(&p.id, last)?;
    let p = c.relink_plan(&p.id)?;
    let limits = Limits {
        page_rows: 1,
        scan_rows: 3,
        ..Limits::default()
    };
    let control = Arc::new(Mutex::new(Control::default()));
    let mut coordinator = Coordinator::default();
    let mut after = None;
    let mut scope = 0;
    let mut exclusions = Vec::new();
    let mut empty_continuations = 0;
    loop {
        let Response::Rules {
            rows,
            next,
            scanned,
        } = coordinator.execute(
            &mut c,
            Request::Rules {
                plan: p.id.clone(),
                revision: I64(p.revision),
                after,
                limit: U64(1),
            },
            &limits,
            &control,
        )?
        else {
            panic!("rules")
        };
        assert!(scanned.0 <= 3);
        if rows.is_empty() && next.is_some() {
            empty_continuations += 1;
        }
        for row in rows {
            match row.rule {
                Rule::Scope(_) => scope += 1,
                Rule::Override(Override::Source {
                    source_id,
                    candidates,
                }) => {
                    assert_eq!(row.label.as_deref(), Some("sidecar 12"));
                    assert!(candidates.is_empty());
                    exclusions.push(source_id.0);
                }
                other => panic!("unexpected rule {other:?}"),
            }
        }
        let Some(next) = next else { break };
        after = Some(next);
    }
    assert_eq!(scope, 1);
    assert_eq!(exclusions, [last]);
    assert!(empty_continuations > 0);
    let details:Vec<String>=c.db.prepare("EXPLAIN QUERY PLAN SELECT source_id FROM storage_source_items WHERE plan=?1 AND (sequence,source_id)>(?2,?3) ORDER BY sequence,source_id LIMIT 1")?.query_map(rusqlite::params![p.id,0,0],|r|r.get(3))?.collect::<rusqlite::Result<_>>()?;
    assert!(details.iter().any(|s| s.contains("storage_source_parent")));
    assert!(!details.iter().any(|s| s.contains("TEMP B-TREE")));
    let revised = c.revise_relink(
        &p.id,
        p.revision,
        vec![
            core::RelinkOverride::Prefix {
                from: core::PathReference::native(&f.originals),
                destinations: vec![NativePath::from_path(f.destination.parent().unwrap())],
            },
            core::RelinkOverride::Asset {
                asset_id: f.key.asset_id.clone(),
                candidates: vec![],
            },
            core::RelinkOverride::Source {
                source_id: last,
                candidates: vec![],
            },
        ],
    )?;
    let Response::Rules { rows, next, .. } = coordinator.execute(
        &mut c,
        Request::Rules {
            plan: revised.id.clone(),
            revision: I64(revised.revision),
            after: None,
            limit: U64(100),
        },
        &Limits::default(),
        &control,
    )?
    else {
        panic!("revised rules")
    };
    assert!(next.is_none());
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter()
            .any(|r| matches!(&r.rule, Rule::Override(Override::Prefix { .. })))
    );
    assert!(rows.iter().any(
        |r| matches!(&r.rule, Rule::Override(Override::Asset { .. }))
            && r.label.as_ref().is_some_and(|l| l.ends_with("seed.png"))
    ));
    let cursor = RuleCursor {
        plan: p.id,
        revision: I64(p.revision),
        stage: RuleStage::Scope,
        position: I64(0),
        source: I64(0),
        entity: String::new(),
    };
    assert!(matches!(
        coordinator
            .execute(
                &mut c,
                Request::Rules {
                    plan: revised.id,
                    revision: I64(revised.revision),
                    after: Some(cursor),
                    limit: U64(1)
                },
                &Limits::default(),
                &control,
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleSession
    ));
    Ok(())
}

#[test]
fn held_relink_preparation_preserves_foreground_edits_and_scoped_cancel() -> Result<()> {
    let f = fixture()?;
    let (entered, events) = mpsc::channel();
    let checkpoint = Arc::new(move |stage: &str, cancel: &AtomicBool| {
        if stage == "relink_preparation" {
            let _ = entered.send(());
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    let (bridge, token) = open(&f, Some(checkpoint))?;
    let p = begin(&f, &bridge, &token)?;
    let work = operation(relink(
        &bridge,
        &token,
        Request::Prepare {
            plan: p.id.clone(),
            revision: p.revision,
            batch_rows: U64(1),
        },
    )?);
    events.recv_timeout(Duration::from_secs(5))?;
    assert!(matches!(
        call(
            &bridge,
            app::Request::Image {
                catalog: token.clone(),
                key: f.key.clone()
            }
        )?,
        app::Response::Image(_)
    ));
    let app::Response::Variant(saved) = call(
        &bridge,
        app::Request::SaveRecipe {
            catalog: token.clone(),
            key: f.key.clone(),
            expected_revision: I64(0),
            recipe: crate::edit::Recipe::V1(crate::edit::RecipeV1 {
                exposure_ev: 1.0,
                ..Default::default()
            }),
        },
    )?
    else {
        anyhow::bail!("save")
    };
    assert!(
        relink(
            &bridge,
            "stale-catalog",
            Request::Cancel {
                operation: work.id.clone()
            }
        )
        .is_err()
    );
    assert!(
        relink(
            &bridge,
            &token,
            Request::Cancel {
                operation: "stale-operation".into()
            }
        )
        .is_err()
    );
    let s = operation(relink(
        &bridge,
        &token,
        Request::Status {
            operation: Some(work.id.clone()),
        },
    )?);
    assert_eq!(s.phase, Phase::Preparing);
    relink(
        &bridge,
        &token,
        Request::Cancel {
            operation: work.id.clone(),
        },
    )?;
    assert_eq!(terminal(&bridge, &token, &work.id)?.phase, Phase::Canceled);
    let fresh = operation(relink(
        &bridge,
        &token,
        Request::Revise {
            plan: p.id.clone(),
            revision: p.revision,
            changes: vec![],
        },
    )?);
    let fresh = terminal(&bridge, &token, &fresh.id)?;
    assert_eq!(fresh.phase, Phase::Complete);
    let fresh = fresh.plan.unwrap();
    assert_ne!(fresh.id, p.id);
    assert_eq!(fresh.state, "preparing");
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(&f.root)?;
    assert_eq!(c.edit_variant(&f.key)?.recipe_digest, saved.recipe_digest);
    assert_eq!(c.relink_plan(&p.id)?.state, "preparing");
    assert_eq!(std::fs::read(f.originals.join("seed.png"))?, f.bytes);
    assert_eq!(std::fs::read(&f.destination)?, f.bytes);
    Ok(())
}

#[test]
fn original_observation_is_offactor_and_bound_to_the_selected_variant() -> Result<()> {
    let f = fixture()?;
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let (entered, events) = mpsc::channel();
    let checkpoint = Arc::new(move |stage: &str, cancel: &AtomicBool| {
        if stage == "relink_original" {
            let _ = entered.send(());
            while !released.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    let (bridge, token) = open(&f, Some(checkpoint))?;
    let app::Response::Variant(copy) = call(
        &bridge,
        app::Request::CreateVariant {
            catalog: token.clone(),
            key: f.key.clone(),
            expected_revision: I64(0),
            label: "Selected alternate".into(),
        },
    )?
    else {
        anyhow::bail!("copy")
    };
    let work = operation(relink(
        &bridge,
        &token,
        Request::Original {
            key: copy.key.clone(),
        },
    )?);
    events
        .recv_timeout(Duration::from_secs(5))
        .with_context(|| {
            format!(
                "waiting for original observation checkpoint; current worker status: {:?}",
                relink(
                    &bridge,
                    &token,
                    Request::Status {
                        operation: Some(work.id.clone())
                    }
                )
            )
        })?;
    call(
        &bridge,
        app::Request::SaveRecipe {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: copy.revision,
            recipe: crate::edit::Recipe::V1(crate::edit::RecipeV1 {
                exposure_ev: 1.0,
                ..Default::default()
            }),
        },
    )?;
    release.store(true, Ordering::Release);
    let stale = terminal(&bridge, &token, &work.id)?;
    assert_eq!(stale.phase, Phase::Failed);
    assert!(
        stale
            .error
            .unwrap()
            .contains("changed during original observation")
    );
    let work = operation(relink(
        &bridge,
        &token,
        Request::Original {
            key: copy.key.clone(),
        },
    )?);
    let observed = terminal(&bridge, &token, &work.id)?;
    assert_eq!(observed.phase, Phase::Complete);
    let Outcome::Original(value) = *observed.result.unwrap() else {
        panic!("original")
    };
    assert_eq!(value.key, copy.key);
    assert_eq!(value.status.asset_id, f.key.asset_id);
    let path = f.originals.join("seed.png");
    let missing = f.originals.join("temporarily-away.png");
    std::fs::rename(&path, &missing)?;
    let work = operation(relink(
        &bridge,
        &token,
        Request::Original { key: copy.key },
    )?);
    let observed = terminal(&bridge, &token, &work.id)?;
    std::fs::rename(&missing, &path)?;
    assert_eq!(observed.phase, Phase::Complete);
    let Outcome::Original(value) = *observed.result.unwrap() else {
        panic!("original")
    };
    assert!(!matches!(
        value.status.state.as_str(),
        "available_unverified" | "online_unverified"
    ));
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    assert_eq!(std::fs::read(path)?, f.bytes);
    Ok(())
}

#[test]
fn atomic_relink_status_write_hold_cancel_commit_wins_and_close_undo() -> Result<()> {
    let f = fixture()?;
    let p = ready(&f)?;
    let mode = Arc::new(AtomicUsize::new(0));
    let observed = mode.clone();
    let (entered, events) = mpsc::channel();
    let checkpoint = Arc::new(move |stage: &str, cancel: &AtomicBool| {
        let mode = observed.load(Ordering::Acquire);
        let hold = (stage == "relink_before_mutation" && (mode == 0 || mode == 2))
            || (stage == "relink_committed" && mode == 1);
        if hold {
            let _ = entered.send(stage.to_owned());
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    let (bridge, token) = open(&f, Some(checkpoint))?;
    let work = operation(relink(
        &bridge,
        &token,
        Request::Apply {
            plan: p.id.clone(),
            revision: p.revision,
        },
    )?);
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5))?,
        "relink_before_mutation"
    );
    let s = operation(relink(
        &bridge,
        &token,
        Request::Status {
            operation: Some(work.id.clone()),
        },
    )?);
    assert!(s.write_hold);
    assert_eq!(s.boundary.as_deref(), Some("before_mutation"));
    let app::Response::Metadata(identity) = call(
        &bridge,
        app::Request::Metadata {
            catalog: token.clone(),
            request: Box::new(app::metadata::Request::Identity { key: f.key.clone() }),
        },
    )?
    else {
        panic!("metadata read blocked during relink hold")
    };
    let app::metadata::Response::Identity(identity) = *identity else {
        panic!("metadata identity")
    };
    assert!(matches!(
        call(
            &bridge,
            app::Request::Metadata {
                catalog: token.clone(),
                request: Box::new(app::metadata::Request::Fields {
                    identity: identity.clone(),
                    after: None,
                    limit: 20
                }),
            }
        )?,
        app::Response::Metadata(_)
    ));
    let blocked_metadata = call(
        &bridge,
        app::Request::Metadata {
            catalog: token.clone(),
            request: Box::new(app::metadata::Request::Resolve {
                key: f.key.clone(),
                expected_revision: identity.metadata_revision,
                field: "rating".into(),
                model: I64(1),
            }),
        },
    )
    .unwrap_err();
    assert!(matches!(
        blocked_metadata
            .downcast_ref::<BridgeError>()
            .map(|e| &e.code),
        Some(ErrorCode::Busy)
    ));

    assert!(matches!(
        call(
            &bridge,
            app::Request::Image {
                catalog: token.clone(),
                key: f.key.clone()
            }
        )?,
        app::Response::Image(_)
    ));
    let blocked = call(
        &bridge,
        app::Request::SaveRecipe {
            catalog: token.clone(),
            key: f.key.clone(),
            expected_revision: I64(0),
            recipe: Default::default(),
        },
    )
    .unwrap_err();
    assert!(matches!(
        blocked.downcast_ref::<BridgeError>().map(|e| &e.code),
        Some(ErrorCode::Busy)
    ));
    relink(
        &bridge,
        &token,
        Request::Cancel {
            operation: work.id.clone(),
        },
    )?;
    let s = terminal(&bridge, &token, &work.id)?;
    assert_eq!(s.phase, Phase::Canceled);
    assert!(!s.write_hold);
    mode.store(1, Ordering::Release);
    let work = operation(relink(
        &bridge,
        &token,
        Request::Apply {
            plan: p.id.clone(),
            revision: p.revision,
        },
    )?);
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5))?,
        "relink_committed"
    );
    relink(
        &bridge,
        &token,
        Request::Cancel {
            operation: work.id.clone(),
        },
    )?;
    let s = terminal(&bridge, &token, &work.id)?;
    assert_eq!(s.phase, Phase::Complete);
    let applied = s.plan.unwrap();
    assert_eq!(applied.state, "applied");
    mode.store(2, Ordering::Release);
    let work = operation(relink(
        &bridge,
        &token,
        Request::Undo {
            plan: applied.id.clone(),
            revision: applied.revision,
        },
    )?);
    assert_eq!(
        events.recv_timeout(Duration::from_secs(5))?,
        "relink_before_mutation"
    );
    assert!(
        operation(relink(
            &bridge,
            &token,
            Request::Status {
                operation: Some(work.id)
            }
        )?)
        .write_hold
    );
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let c = Catalog::open(&f.root)?;
    assert_eq!(c.relink_plan(&applied.id)?.state, "applied");
    // Pre-schema12 plans have no derived summary row. Explicit undo remains
    // available under the core's exact lineage checks despite unavailable counts.
    c.db.execute(
        "DELETE FROM storage_review_summary WHERE plan=?",
        [&applied.id],
    )?;
    assert!(!c.relink_plan(&applied.id)?.summary_complete);
    drop(c);
    let (bridge, token) = open(&f, None)?;
    assert!(matches!(
        relink(&bridge, &token, Request::Status { operation: None })?,
        Response::Operation(None)
    ));
    let work = operation(relink(
        &bridge,
        &token,
        Request::Undo {
            plan: applied.id,
            revision: applied.revision,
        },
    )?);
    let s = terminal(&bridge, &token, &work.id)?;
    assert_eq!(s.phase, Phase::Complete);
    assert_eq!(s.plan.unwrap().state, "undone");
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    assert_eq!(std::fs::read(f.originals.join("seed.png"))?, f.bytes);
    assert_eq!(std::fs::read(&f.destination)?, f.bytes);
    Ok(())
}

#[test]
fn restored_hold_allows_reviewed_nested_relink_apply_reopen_and_undo() -> Result<()> {
    let mut f = fixture()?;
    let nested = f.originals.join("nested").join("deeper");
    std::fs::create_dir_all(&nested)?;
    image::RgbImage::from_pixel(16, 12, image::Rgb([90u8, 40, 80]))
        .save(nested.join("second.png"))?;
    let old_root = f.originals.canonicalize()?;
    let mut catalog = Catalog::open(&f.root)?;
    catalog.import(&f.originals, None, |_| Ok(()))?;
    let pending = catalog.begin_photo_export()?;
    let locations = |catalog: &Catalog| -> Result<Vec<(String, Vec<u8>)>> {
        Ok(catalog
            .db
            .prepare("SELECT id,location FROM assets ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    };
    let original_locations = locations(&catalog)?;
    assert_eq!(original_locations.len(), 2);
    drop(catalog);
    let bundle = f._temp.path().join("backup");
    let restored = f._temp.path().join("restored");
    let limits = crate::catalog_backup::Limits::default();
    crate::catalog_backup::backup_catalog(&f.root, &bundle, &limits, |_| Ok(()))?;
    crate::catalog_backup::restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    f.root = restored;
    let moved = f._temp.path().join("relocated");
    std::fs::rename(&f.originals, &moved)?;
    f.originals = moved.canonicalize()?;
    let nested_bytes = std::fs::read(f.originals.join("nested/deeper/second.png"))?;
    let assert_held = |bridge: &Bridge, token: &str| -> Result<()> {
        let app::Response::Restore(Some(status)) = call(
            bridge,
            app::Request::RestoreStatus {
                catalog: token.into(),
            },
        )?
        else {
            anyhow::bail!("restore status missing")
        };
        assert!(status.jobs_held);
        let error = call(
            bridge,
            app::Request::Export {
                catalog: token.into(),
                request: Box::new(app::exports::Request::Begin),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("jobs are held"), "{error}");
        Ok(())
    };
    let (bridge, token) = open(&f, None)?;
    assert_held(&bridge, &token)?;
    let Response::Plan(mut plan) = relink(
        &bridge,
        &token,
        Request::Begin {
            scope: Scope::Prefix {
                from: core::PathReference::native(&old_root),
                destinations: vec![NativePath::from_path(&f.originals)],
            },
        },
    )?
    else {
        anyhow::bail!("plan missing")
    };
    for _ in 0..16 {
        if plan.state == "ready" {
            break;
        }
        let work = operation(relink(
            &bridge,
            &token,
            Request::Prepare {
                plan: plan.id.clone(),
                revision: plan.revision,
                batch_rows: U64(1),
            },
        )?);
        let done = terminal(&bridge, &token, &work.id)?;
        assert_eq!(done.phase, Phase::Complete, "{done:?}");
        plan = done.plan.context("prepared plan")?;
    }
    assert_eq!(plan.state, "ready");
    let Response::Items { rows, .. } = relink(
        &bridge,
        &token,
        Request::Items {
            plan: plan.id.clone(),
            revision: plan.revision,
            after: I64(0),
            limit: U64(10),
        },
    )?
    else {
        anyhow::bail!("review missing")
    };
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.status == "matched"));
    for relative in ["seed.png", "nested/deeper/second.png"] {
        let expected = NativePath::from_path(&f.originals.join(relative));
        assert!(
            rows.iter()
                .any(|row| row.destination.as_ref() == Some(&expected))
        );
    }
    assert!(
        relink(
            &bridge,
            &token,
            Request::Apply {
                plan: plan.id.clone(),
                revision: I64(plan.revision.0 - 1),
            }
        )
        .is_err()
    );
    assert_held(&bridge, &token)?;
    let work = operation(relink(
        &bridge,
        &token,
        Request::Apply {
            plan: plan.id.clone(),
            revision: plan.revision,
        },
    )?);
    let done = terminal(&bridge, &token, &work.id)?;
    assert_eq!(done.phase, Phase::Complete, "{done:?}");
    plan = done.plan.context("applied plan")?;
    assert_eq!(plan.state, "applied");
    assert_held(&bridge, &token)?;
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let catalog = Catalog::open(&f.root)?;
    let applied_locations = locations(&catalog)?;
    assert_eq!(applied_locations.len(), original_locations.len());
    for row in &rows {
        let expected = crate::catalog_storage::encoded_bytes(
            row.destination.as_ref().context("reviewed destination")?,
        );
        assert!(applied_locations.contains(&(row.asset_id.clone(), expected)));
    }
    drop(catalog);
    let (bridge, token) = open(&f, None)?;
    assert_held(&bridge, &token)?;
    let work = operation(relink(
        &bridge,
        &token,
        Request::Undo {
            plan: plan.id,
            revision: plan.revision,
        },
    )?);
    let done = terminal(&bridge, &token, &work.id)?;
    assert_eq!(done.phase, Phase::Complete, "{done:?}");
    assert_eq!(done.plan.context("undone plan")?.state, "undone");
    assert_held(&bridge, &token)?;
    call(&bridge, app::Request::Close { catalog: token })?;
    bridge.shutdown();
    let catalog = Catalog::open(&f.root)?;
    assert_eq!(locations(&catalog)?, original_locations);
    assert_eq!(
        serde_json::to_value(catalog.photo_export_job(&pending.id)?)?,
        serde_json::to_value(pending)?
    );
    assert!(
        catalog
            .restore_status()?
            .context("restore marker")?
            .jobs_held
    );
    assert_eq!(std::fs::read(f.originals.join("seed.png"))?, f.bytes);
    assert_eq!(
        std::fs::read(f.originals.join("nested/deeper/second.png"))?,
        nested_bytes
    );
    Ok(())
}
