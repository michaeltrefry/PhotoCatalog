use anyhow::{Context, Result, bail, ensure};
use photocatalog::{
    Catalog,
    application::*,
    catalog_edits::VariantKey,
    edit::{Recipe, RecipeV1},
    preview::{PreviewPolicy, ServiceLimits},
    storage_volume::NativePath,
};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn config(originals: &Path) -> Config {
    Config {
        worker_executable: PathBuf::from(env!("CARGO_BIN_EXE_photocatalog")),
        cache_root: None,
        original_roots: vec![originals.into()],
        preview_policy: PreviewPolicy::default(),
        preview_limits: ServiceLimits::default(),
        limits: Limits::default(),
    }
}
fn call(b: &Bridge, r: Request) -> Result<Response> {
    match b.submit(r)?.recv() {
        Reply::Ok { value } => Ok(value),
        Reply::Error { error } => Err(error.into()),
    }
}
fn status(b: &Bridge) -> Result<Status> {
    let Response::Status(s) = call(b, Request::Status)? else {
        bail!("wrong status")
    };
    Ok(s)
}
fn wait_ready(b: &Bridge) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let s = status(b)?;
        if matches!(s.phase, Phase::Ready) {
            return Ok(());
        }
        ensure!(!matches!(s.phase, Phase::Failed), "{:?}", s.message);
        ensure!(Instant::now() < until, "index deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn variant(b: &Bridge, token: &str, key: &VariantKey) -> Result<Variant> {
    let Response::Variant(v) = call(
        b,
        Request::Variant {
            catalog: token.into(),
            key: key.clone(),
        },
    )?
    else {
        bail!("wrong variant")
    };
    Ok(v)
}
fn preview(b: &Bridge, token: &str, key: &VariantKey, generation: u64) -> Result<PreviewStatus> {
    let Response::Preview(p) = call(
        b,
        Request::Preview {
            catalog: token.into(),
            key: key.clone(),
            tier: PreviewTier::Thumbnail,
            interactive: false,
            viewport: "selected".into(),
            generation: U64(generation),
            foreground: true,
        },
    )?
    else {
        bail!("wrong preview")
    };
    preview_ready(b, token, p)
}
fn preview_ready(b: &Bridge, token: &str, p: PreviewStatus) -> Result<PreviewStatus> {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let Response::Preview(p) = call(
            b,
            Request::PreviewStatus {
                catalog: token.into(),
                ticket: p.ticket.clone(),
            },
        )?
        else {
            bail!("wrong preview status")
        };
        if matches!(p.state, PreviewState::Ready) {
            return Ok(p);
        }
        ensure!(
            matches!(p.state, PreviewState::Queued),
            "unexpected preview {:?}: {:?}",
            p.state,
            p.message
        );
        ensure!(Instant::now() < until, "preview deadline");
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn real_actor_pages_edits_independent_variants_delivers_binary_and_reopens() -> Result<()> {
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = temp.path().join("originals");
    std::fs::create_dir_all(originals.join("nested 雪"))?;
    let path = originals.join("nested 雪/photo.png");
    image::RgbImage::from_pixel(64, 48, image::Rgb([40u8, 60, 90])).save(&path)?;
    let original = std::fs::read(&path)?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    drop(c);
    let b = Bridge::spawn(config(&originals))?;
    let Response::Status(open) = call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        bail!("wrong open")
    };
    let token = open.catalog.unwrap();
    wait_ready(&b)?;
    let Response::Folders { rows, .. } = call(
        &b,
        Request::Folders {
            catalog: token.clone(),
            parent: None,
            after: I64(0),
            limit: 1,
        },
    )?
    else {
        bail!("wrong folders")
    };
    assert_eq!(rows.len(), 1);
    let Response::Images { rows, .. } = call(
        &b,
        Request::Images {
            catalog: token.clone(),
            folder: None,
            recursive: false,
            text: None,
            cursor: None,
            limit: 1,
        },
    )?
    else {
        bail!("wrong images")
    };
    assert_eq!(rows[0].key, master);
    let Response::Variant(copy) = call(
        &b,
        Request::CreateVariant {
            catalog: token.clone(),
            key: master.clone(),
            expected_revision: I64(0),
            label: "Independent".into(),
        },
    )?
    else {
        bail!("wrong create")
    };
    let Response::Image(selected) = call(
        &b,
        Request::Image {
            catalog: token.clone(),
            key: copy.key.clone(),
        },
    )?
    else {
        bail!("wrong selected image")
    };
    assert_eq!(selected.key, copy.key);
    assert_ne!(selected.image_id, master.asset_id);
    call(
        &b,
        Request::Cull {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: selected.metadata_revision,
            operation: CullOperation::Flag(photocatalog::organization::Flag::Pick),
        },
    )?;
    let Response::Image(selected) = call(
        &b,
        Request::Image {
            catalog: token.clone(),
            key: copy.key.clone(),
        },
    )?
    else {
        bail!("wrong selected image")
    };
    assert_eq!(selected.flag, "pick");
    let Response::Image(master_image) = call(
        &b,
        Request::Image {
            catalog: token.clone(),
            key: master.clone(),
        },
    )?
    else {
        bail!("wrong master image")
    };
    assert_eq!(master_image.flag, "unflagged");
    let changed = Recipe::V1(RecipeV1 {
        exposure_ev: 1.0,
        ..Default::default()
    });
    let Response::Variant(saved) = call(
        &b,
        Request::SaveRecipe {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: I64(0),
            recipe: changed.clone(),
        },
    )?
    else {
        bail!("wrong save")
    };
    assert_eq!(saved.revision, I64(1));
    assert!(
        call(
            &b,
            Request::SaveRecipe {
                catalog: token.clone(),
                key: copy.key.clone(),
                expected_revision: I64(0),
                recipe: changed
            }
        )
        .is_err()
    );
    assert_eq!(variant(&b, &token, &master)?.revision, I64(0));
    wait_ready(&b)?;
    let Response::Images { rows, .. } = call(
        &b,
        Request::Images {
            catalog: token.clone(),
            folder: None,
            recursive: false,
            text: None,
            cursor: None,
            limit: 10,
        },
    )?
    else {
        bail!("wrong images")
    };
    assert_eq!(rows.len(), 2);
    let a = preview(&b, &token, &master, 1)?;
    let a_bytes = b
        .preview_bytes(token.clone(), a.ticket.clone(), true)?
        .recv()?;
    let a_pixels = image::load_from_memory(a_bytes.bytes())?.to_rgb8();
    // Keep one transport lease while another native preview is produced.
    let p = preview(&b, &token, &copy.key, 2)?;
    let bytes = b
        .preview_bytes(token.clone(), p.ticket.clone(), false)?
        .recv()?;
    assert_eq!(bytes.mime, "image/jpeg");
    let pixels = image::load_from_memory(bytes.bytes())?.to_rgb8();
    assert_ne!(a_pixels.as_raw(), pixels.as_raw());
    drop(bytes);
    drop(a_bytes);
    let Response::Preview(cancel) = call(
        &b,
        Request::CancelPreview {
            catalog: token.clone(),
            ticket: p.ticket.clone(),
        },
    )?
    else {
        bail!("wrong cancel")
    };
    assert!(matches!(
        cancel.state,
        PreviewState::Canceled | PreviewState::CancelRequested
    ));
    assert!(
        b.preview_bytes(token.clone(), p.ticket, true)?
            .recv()
            .is_err()
    );
    let Response::Variant(undo) = call(
        &b,
        Request::Undo {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: saved.revision,
        },
    )?
    else {
        bail!("wrong undo")
    };
    assert_ne!(undo.recipe_digest, saved.recipe_digest);
    let Response::Variant(redo) = call(
        &b,
        Request::Redo {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: undo.revision,
        },
    )?
    else {
        bail!("wrong redo")
    };
    assert_eq!(redo.recipe_digest, saved.recipe_digest);
    let Response::History { rows, .. } = call(
        &b,
        Request::History {
            catalog: token.clone(),
            key: copy.key.clone(),
            after: I64(-1),
            limit: 10,
        },
    )?
    else {
        bail!("wrong history")
    };
    assert!(rows.len() >= 3);
    call(
        &b,
        Request::Close {
            catalog: token.clone(),
        },
    )?;
    assert!(
        call(
            &b,
            Request::Variant {
                catalog: token,
                key: master.clone()
            }
        )
        .is_err()
    );
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let newtoken = status(&b)?.catalog.unwrap();
    assert_eq!(
        variant(&b, &newtoken, &copy.key)?.recipe_digest,
        saved.recipe_digest
    );
    b.shutdown();
    assert_eq!(std::fs::read(path)?, original);
    let c = Catalog::open(root)?;
    assert_eq!(
        c.edit_variant(&copy.key)?.recipe_digest,
        saved.recipe_digest
    );
    Ok(())
}
#[test]
fn existing_open_never_creates_and_new_catalog_is_explicit() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let b = Bridge::spawn(config(&originals))?;
    let missing = temp.path().join("missing");
    assert!(
        call(
            &b,
            Request::OpenExisting {
                path: NativePath::from_path(&missing)
            }
        )
        .is_err()
    );
    assert!(!missing.exists());
    let inside = originals.join("catalog");
    assert!(
        call(
            &b,
            Request::Create {
                path: NativePath::from_path(&inside)
            }
        )
        .is_err()
    );
    assert!(!inside.exists());
    let Response::Status(s) = call(
        &b,
        Request::Create {
            path: NativePath::from_path(&missing),
        },
    )?
    else {
        bail!("wrong create")
    };
    assert!(missing.join("catalog.sqlite3").is_file());
    let token = s.catalog.unwrap();
    call(&b, Request::Close { catalog: token })?;
    assert!(
        call(
            &b,
            Request::Create {
                path: NativePath::from_path(&missing)
            }
        )
        .is_err()
    );
    b.shutdown();
    Ok(())
}
#[test]
fn close_reaps_an_observed_active_native_preview_before_cache_reopen() -> Result<()> {
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(1024, 768, image::Rgb([20u8, 60, 120]))
        .save(originals.join("photo.png"))?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    let key = c.create_edit_variant(&master, 0, "native worker")?.key;
    drop(c);
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let Response::Preview(p) = call(
        &b,
        Request::Preview {
            catalog: token.clone(),
            key,
            tier: PreviewTier::Large,
            interactive: false,
            viewport: "selected".into(),
            generation: U64(1),
            foreground: true,
        },
    )?
    else {
        bail!("wrong preview")
    };
    ensure!(
        matches!(p.state, PreviewState::Queued),
        "fresh variant unexpectedly cached"
    );
    let until = Instant::now() + Duration::from_secs(10);
    while status(&b)?.active_previews == 0 {
        ensure!(Instant::now() < until, "native worker never became active");
        std::thread::yield_now();
    }
    call(&b, Request::Close { catalog: token })?;
    assert!(matches!(status(&b)?.phase, Phase::Closed));
    assert_eq!(status(&b)?.active_previews, 0);
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    b.shutdown();
    Ok(())
}

#[test]
fn app_cache_parent_is_namespaced_by_catalog_and_reused_on_reopen() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let cache = temp.path().join("app-cache");
    let mut cfg = config(&originals);
    cfg.cache_root = Some(cache.clone());
    let b = Bridge::spawn(cfg)?;
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    for path in [&first, &second] {
        call(
            &b,
            Request::Create {
                path: NativePath::from_path(path),
            },
        )?;
        let token = status(&b)?.catalog.unwrap();
        call(&b, Request::Close { catalog: token })?;
    }
    let names = || -> Result<Vec<PathBuf>> {
        let mut names = std::fs::read_dir(&cache)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    let before = names()?;
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|p| p.join("manifest").is_dir()));
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&first),
        },
    )?;
    b.shutdown();
    assert_eq!(names()?, before);
    Ok(())
}

#[test]
fn opaque_image_cursors_support_back_forward_and_reject_stale_epoch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    for n in 0..4 {
        image::RgbImage::from_pixel(8, 8, image::Rgb([20u8 + n, 40, 70]))
            .save(originals.join(format!("{n}.png")))?;
    }
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    drop(c);
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let page = |cursor: Option<String>| -> Result<(GridImage, Option<String>)> {
        let Response::Images { mut rows, next, .. } = call(
            &b,
            Request::Images {
                catalog: token.clone(),
                folder: None,
                recursive: false,
                text: None,
                cursor,
                limit: 1,
            },
        )?
        else {
            bail!("wrong page")
        };
        ensure!(rows.len() == 1, "missing page row");
        Ok((rows.remove(0), next))
    };
    let (first, cursor1) = page(None)?;
    let (second, cursor2) = page(cursor1.clone())?;
    let (third, _) = page(cursor2.clone())?;
    assert_ne!(first.image_id, second.image_id);
    assert_ne!(second.image_id, third.image_id);
    assert_eq!(page(cursor1.clone())?.0.image_id, second.image_id);
    assert_eq!(page(cursor2)?.0.image_id, third.image_id);
    call(
        &b,
        Request::Cull {
            catalog: token.clone(),
            key: first.key,
            expected_revision: first.metadata_revision,
            operation: CullOperation::Rating(4),
        },
    )?;
    wait_ready(&b)?;
    assert!(page(cursor1).is_err());
    b.shutdown();
    Ok(())
}

#[test]
fn desktop_search_filters_keep_variant_identity_and_reject_changed_query_cursor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    for name in ["a.png", "z.png"] {
        image::RgbImage::from_pixel(8, 8, image::Rgb([30u8, 50, 80])).save(originals.join(name))?;
    }
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(c.browse(0, 10)?[0].id.clone());
    drop(c);
    let bridge = Bridge::spawn(config(&originals))?;
    let Response::Status(open) = call(
        &bridge,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        bail!("open response");
    };
    let token = open.catalog.unwrap();
    wait_ready(&bridge)?;
    let original_variant = variant(&bridge, &token, &master)?;
    let Response::Variant(copy) = call(
        &bridge,
        Request::CreateVariant {
            catalog: token.clone(),
            key: master.clone(),
            expected_revision: original_variant.revision,
            label: "Independent".into(),
        },
    )?
    else {
        bail!("copy response");
    };
    let Response::Image(row) = call(
        &bridge,
        Request::Image {
            catalog: token.clone(),
            key: copy.key.clone(),
        },
    )?
    else {
        bail!("image response");
    };
    let Response::Culled { metadata_revision } = call(
        &bridge,
        Request::Cull {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: row.metadata_revision,
            operation: CullOperation::Rating(5),
        },
    )?
    else {
        bail!("rating response");
    };
    call(
        &bridge,
        Request::Cull {
            catalog: token.clone(),
            key: copy.key.clone(),
            expected_revision: metadata_revision,
            operation: CullOperation::Flag(photocatalog::organization::Flag::Pick),
        },
    )?;
    let Response::Images { rows, .. } = call(
        &bridge,
        Request::Search {
            catalog: token.clone(),
            options: Box::new(browse::Options {
                rating: Some(5),
                flag: Some(photocatalog::organization::Flag::Pick),
                ..Default::default()
            }),
            cursor: None,
            limit: 10,
        },
    )?
    else {
        bail!("filter response");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, copy.key);
    let options = browse::Options {
        sort: photocatalog::organization_search::Sort::Filename,
        direction: photocatalog::organization_search::Direction::Descending,
        ..Default::default()
    };
    let Response::Images { rows, next, .. } = call(
        &bridge,
        Request::Search {
            catalog: token.clone(),
            options: Box::new(options.clone()),
            cursor: None,
            limit: 1,
        },
    )?
    else {
        bail!("sorted response");
    };
    assert_eq!(rows[0].filename, "z.png");
    ensure!(next.is_some(), "expected page cursor");
    let failed = call(
        &bridge,
        Request::Search {
            catalog: token.clone(),
            options: Box::new(browse::Options {
                rating: Some(5),
                ..options
            }),
            cursor: next,
            limit: 1,
        },
    );
    ensure!(failed.is_err(), "cursor must remain bound to exact query");
    assert_eq!(
        variant(&bridge, &token, &master)?.revision,
        original_variant.revision
    );
    bridge.shutdown();
    Ok(())
}

fn import_status(b: &Bridge, token: &str) -> Result<Option<ImportStatus>> {
    let Response::Import(s) = call(
        b,
        Request::ImportStatus {
            catalog: token.into(),
        },
    )?
    else {
        bail!("wrong import status")
    };
    Ok(s)
}
fn wait_import(b: &Bridge, token: &str, terminal: ImportPhase) -> Result<ImportStatus> {
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let s = import_status(b, token)?.unwrap();
        if s.phase == terminal {
            return Ok(s);
        }
        ensure!(
            !matches!(
                s.phase,
                ImportPhase::Failed | ImportPhase::Canceled | ImportPhase::Complete
            ),
            "unexpected import {:?}: {:?}",
            s.phase,
            s.error
        );
        ensure!(Instant::now() < until, "import deadline: {:?}", s);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn incremental_import_interleaves_edits_cancels_rewalks_and_preserves_originals() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir_all(originals.join("nested 雪/deeper"))?;
    let seed_path = originals.join("seed.png");
    image::RgbImage::from_pixel(32, 24, image::Rgb([20u8, 40, 70])).save(&seed_path)?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let seed = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    drop(c);
    let mut originals_saved = vec![(seed_path.clone(), std::fs::read(&seed_path)?)];
    let sidecar_path = originals.join("seed.xmp");
    let sidecar = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:vendor="urn:private:fixture" vendor:opaque="preserve this exact packet"/></rdf:RDF></x:xmpmeta>"#;
    std::fs::write(&sidecar_path, sidecar)?;
    originals_saved.push((sidecar_path, sidecar.to_vec()));

    for n in 0..48 {
        let path = originals.join(format!("nested 雪/deeper/{n}.png"));
        image::RgbImage::from_pixel(64, 48, image::Rgb([n as u8, 50, 80])).save(&path)?;
        originals_saved.push((path.clone(), std::fs::read(&path)?));
        let mut permissions = std::fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions)?;
    }
    std::fs::write(originals.join("note.txt"), b"not imported")?;
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let Response::Import(Some(start)) = call(
        &b,
        Request::ImportStart {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
    )?
    else {
        bail!("wrong import start")
    };
    assert_eq!(
        start.source,
        NativePath::from_path(&originals.canonicalize()?)
    );
    // Real foreground query and edit complete while the incremental re-walk remains active.
    let Response::Image(image) = call(
        &b,
        Request::Image {
            catalog: token.clone(),
            key: seed.clone(),
        },
    )?
    else {
        bail!("wrong image")
    };
    assert_eq!(image.key, seed);
    let changed = Recipe::V1(RecipeV1 {
        exposure_ev: 1.0,
        ..RecipeV1::default()
    });
    let Response::Variant(saved) = call(
        &b,
        Request::SaveRecipe {
            catalog: token.clone(),
            key: seed.clone(),
            expected_revision: I64(0),
            recipe: changed,
        },
    )?
    else {
        bail!("wrong save")
    };
    assert!(matches!(
        import_status(&b, &token)?.unwrap().phase,
        ImportPhase::Discovering | ImportPhase::Draining
    ));
    let until = Instant::now() + Duration::from_secs(10);
    while import_status(&b, &token)?.unwrap().pending_previews == 0 {
        ensure!(Instant::now() < until, "no import work admitted");
        std::thread::sleep(Duration::from_millis(2));
    }
    // A visible preview has separate ownership from import consumers.
    let Response::Preview(visible) = call(
        &b,
        Request::Preview {
            catalog: token.clone(),
            key: seed.clone(),
            tier: PreviewTier::Large,
            interactive: false,
            viewport: "current".into(),
            generation: U64(1),
            foreground: true,
        },
    )?
    else {
        bail!("wrong visible preview")
    };
    let Response::Import(Some(cancel)) = call(
        &b,
        Request::ImportCancel {
            catalog: token.clone(),
            import: start.id.clone(),
        },
    )?
    else {
        bail!("wrong cancel")
    };
    assert_eq!(cancel.phase, ImportPhase::CancelRequested);
    wait_import(&b, &token, ImportPhase::Canceled)?;
    let visible = preview_ready(&b, &token, visible)?;
    assert!(matches!(visible.state, PreviewState::Ready));
    call(&b, Request::Close { catalog: token })?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    assert!(import_status(&b, &token)?.is_none());
    let Response::Import(Some(resumed)) = call(
        &b,
        Request::ImportResume {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
    )?
    else {
        bail!("wrong resume")
    };
    assert_ne!(resumed.id, start.id);
    assert!(
        call(
            &b,
            Request::ImportCancel {
                catalog: token.clone(),
                import: start.id
            }
        )
        .is_err()
    );
    let complete = wait_import(&b, &token, ImportPhase::Complete)?;
    assert_eq!(complete.imported.0 + complete.unchanged.0, 49);
    assert_eq!(complete.failed.0, 0);
    assert!(complete.skipped.0 >= 1);
    assert_eq!(
        variant(&b, &token, &seed)?.recipe_digest,
        saved.recipe_digest
    );
    let Response::Images { rows, .. } = call(
        &b,
        Request::Images {
            catalog: token.clone(),
            folder: None,
            recursive: true,
            text: None,
            cursor: None,
            limit: 100,
        },
    )?
    else {
        bail!("wrong imported page")
    };
    assert_eq!(rows.len(), 49);
    let Response::Folders { rows: folders, .. } = call(
        &b,
        Request::Folders {
            catalog: token.clone(),
            parent: None,
            after: I64(0),
            limit: 100,
        },
    )?
    else {
        bail!("wrong folders")
    };
    assert!(!folders.is_empty());
    b.shutdown();
    let c = Catalog::open(&root)?;
    assert_eq!(c.browse(0, 100)?.len(), 49);
    assert_eq!(c.edit_variant(&seed)?.revision, saved.revision.0);
    assert!(c.edit_variant(&seed)?.can_undo);
    assert_eq!(
        c.render_identity(&seed.asset_id)?.fingerprint,
        Some(
            blake3::hash(&std::fs::read(&seed_path)?)
                .to_hex()
                .to_string()
        )
    );
    let retained = c
        .metadata_history(&seed.asset_id, 0, 100)?
        .into_iter()
        .map(|h| c.metadata_packets(&seed.asset_id, h.id))
        .collect::<Result<Vec<_>>>()?;
    assert!(
        retained
            .iter()
            .flatten()
            .any(|packet| packet.bytes == sidecar)
    );

    for (path, bytes) in originals_saved {
        assert_eq!(std::fs::read(path)?, bytes);
    }
    // Restore fixture permissions for cross-platform temporary-directory cleanup.
    for entry in std::fs::read_dir(originals.join("nested 雪/deeper"))? {
        let path = entry?.path();
        let permissions = std::fs::metadata(&seed_path)?.permissions();
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[test]
fn imports_report_bad_files_and_respect_restored_job_hold() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    std::fs::write(originals.join("bad.png"), b"invalid png")?;
    let root = temp.path().join("catalog");
    drop(Catalog::open(&root)?);
    let bundle = temp.path().join("bundle");
    let restored = temp.path().join("restored");
    let limits = photocatalog::catalog_backup::Limits::default();
    photocatalog::catalog_backup::backup_catalog(&root, &bundle, &limits, |_| Ok(()))?;
    photocatalog::catalog_backup::restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&restored),
        },
    )?;
    let token = status(&b)?.catalog.unwrap();
    assert!(status(&b)?.jobs_held);
    assert!(
        call(
            &b,
            Request::ImportStart {
                catalog: token.clone(),
                source: NativePath::from_path(&originals)
            }
        )
        .is_err()
    );
    assert!(import_status(&b, &token)?.is_none());
    call(&b, Request::Close { catalog: token })?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    let token = status(&b)?.catalog.unwrap();
    call(
        &b,
        Request::ImportStart {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
    )?;
    let report = wait_import(&b, &token, ImportPhase::Complete)?;
    assert_eq!(report.failed.0, 1);
    assert!(report.error.is_some());
    assert_eq!(report.pending_previews, 0);
    b.shutdown();
    assert_eq!(std::fs::read(originals.join("bad.png"))?, b"invalid png");
    Ok(())
}

#[test]
fn changed_edited_original_import_fails_actionably_without_reset_or_duplicate() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("one.png");
    image::RgbImage::from_pixel(24, 16, image::Rgb([10u8, 20, 30])).save(&path)?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let key = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    c.save_edit_recipe(
        &key,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.0,
            ..RecipeV1::default()
        }),
    )?;
    let source_before = serde_json::to_value(c.render_identity(&key.asset_id)?)?;
    let edit_before = serde_json::to_value(c.edit_variant(&key)?)?;
    let history_before = serde_json::to_value(c.edit_history(&key, -1, 100)?)?;
    let metadata_before = serde_json::to_value(c.metadata_history(&key.asset_id, 0, 100)?)?;
    drop(c);
    image::RgbImage::from_pixel(24, 16, image::Rgb([90u8, 80, 70])).save(&path)?;
    let changed_original = std::fs::read(&path)?;
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    call(
        &b,
        Request::ImportStart {
            catalog: token.clone(),
            source: NativePath::from_path(&originals),
        },
    )?;
    let failed = wait_import(&b, &token, ImportPhase::Failed)?;
    assert_eq!(failed.failed.0, 1);
    assert!(failed.error.unwrap().contains("source-changed"));
    assert_eq!(
        failed.error_source,
        Some(NativePath::from_path(&path.canonicalize()?))
    );
    assert_eq!(failed.imported.0, 0);
    b.shutdown();
    let c = Catalog::open(&root)?;
    assert_eq!(c.browse(0, 100)?.len(), 1);
    assert_eq!(
        serde_json::to_value(c.render_identity(&key.asset_id)?)?,
        source_before
    );
    assert_eq!(serde_json::to_value(c.edit_variant(&key)?)?, edit_before);
    assert_eq!(
        serde_json::to_value(c.edit_history(&key, -1, 100)?)?,
        history_before
    );
    assert_eq!(
        serde_json::to_value(c.metadata_history(&key.asset_id, 0, 100)?)?,
        metadata_before
    );
    assert_eq!(std::fs::read(&path)?, changed_original);
    Ok(())
}

#[test]
fn desktop_backup_restore_uses_selected_catalog_and_explicit_job_admission() -> Result<()> {
    fn wait_backup(bridge: &Bridge, operation: &str) -> Result<backup::Snapshot> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let Response::Backup(Some(snapshot)) = call(bridge, Request::BackupStatus)? else {
                bail!("backup status response");
            };
            ensure!(snapshot.operation == operation, "backup identity changed");
            if snapshot.state == backup::State::Complete {
                return Ok(snapshot);
            }
            ensure!(
                snapshot.state != backup::State::Failed,
                "{:?}",
                snapshot.error
            );
            ensure!(Instant::now() < deadline, "backup deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("catalog");
    let bundle = temp.path().join("backup");
    let destination = temp.path().join("restored");
    std::fs::create_dir(temp.path().join("originals"))?;
    let bridge = Bridge::spawn(config(&temp.path().join("originals")))?;
    let Response::Status(open) = call(
        &bridge,
        Request::Create {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        bail!("create response");
    };
    let catalog = open.catalog.unwrap();
    wait_ready(&bridge)?;
    ensure!(
        call(
            &bridge,
            Request::BackupCreate {
                catalog: "stale-session".into(),
                bundle: NativePath::from_path(&bundle)
            }
        )
        .is_err(),
        "stale backup admitted"
    );
    ensure!(!bundle.exists(), "stale request created backup");
    let Response::Backup(Some(start)) = call(
        &bridge,
        Request::BackupCreate {
            catalog: catalog.clone(),
            bundle: NativePath::from_path(&bundle),
        },
    )?
    else {
        bail!("backup start response");
    };
    let saved = wait_backup(&bridge, &start.operation)?;
    let Some(backup::Receipt::Backup(saved_receipt)) = saved.receipt else {
        bail!("backup receipt");
    };
    call(&bridge, Request::Close { catalog })?;
    let Response::Backup(Some(inspect)) = call(
        &bridge,
        Request::BackupInspect {
            bundle: NativePath::from_path(&bundle),
        },
    )?
    else {
        bail!("inspect response");
    };
    let inspected = wait_backup(&bridge, &inspect.operation)?;
    let Some(backup::Receipt::Backup(inspected_receipt)) = inspected.receipt else {
        bail!("inspection receipt");
    };
    assert_eq!(
        saved_receipt.database_blake3,
        inspected_receipt.database_blake3
    );
    let Response::Backup(Some(start)) = call(
        &bridge,
        Request::BackupRestore {
            bundle: NativePath::from_path(&bundle),
            destination: NativePath::from_path(&destination),
        },
    )?
    else {
        bail!("restore start response");
    };
    let restored = wait_backup(&bridge, &start.operation)?;
    let Some(backup::Receipt::Restore(receipt)) = restored.receipt else {
        bail!("restore receipt");
    };
    let Response::Status(open) = call(
        &bridge,
        Request::OpenExisting {
            path: NativePath::from_path(&destination),
        },
    )?
    else {
        bail!("open restored response");
    };
    let catalog = open.catalog.unwrap();
    wait_ready(&bridge)?;
    let Response::Restore(Some(held)) = call(
        &bridge,
        Request::RestoreStatus {
            catalog: catalog.clone(),
        },
    )?
    else {
        bail!("restore status response");
    };
    assert!(held.jobs_held);
    assert_eq!(held.receipt.restore_id, receipt.restore_id);
    for (id, acknowledge) in [
        (receipt.restore_id.clone(), false),
        ("stale-restore".into(), true),
    ] {
        ensure!(
            call(
                &bridge,
                Request::ResumeRestoredJobs {
                    catalog: catalog.clone(),
                    restore_id: id,
                    acknowledge_pending_jobs: acknowledge
                }
            )
            .is_err(),
            "invalid restore acknowledgment admitted"
        );
    }
    assert!(status(&bridge)?.jobs_held);
    let Response::Restore(Some(released)) = call(
        &bridge,
        Request::ResumeRestoredJobs {
            catalog: catalog.clone(),
            restore_id: receipt.restore_id,
            acknowledge_pending_jobs: true,
        },
    )?
    else {
        bail!("resume response");
    };
    assert!(!released.jobs_held);
    assert!(!status(&bridge)?.jobs_held);
    bridge.shutdown();
    Ok(())
}

fn pending_imported_fixture(base: &Path) -> Result<(PathBuf, PathBuf, VariantKey, VariantKey)> {
    pending_imported_fixture_with_digest(base, true)
}
fn pending_imported_fixture_with_digest(
    base: &Path,
    inspect_original: bool,
) -> Result<(PathBuf, PathBuf, VariantKey, VariantKey)> {
    let originals = base.join("originals");
    std::fs::create_dir(&originals)?;
    let originals = originals.canonicalize()?;
    let path = originals.join("source.png");
    image::RgbImage::from_pixel(48, 32, image::Rgb([30u8, 55, 80])).save(&path)?;
    std::fs::write(originals.join("source.xmp"), br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:custom="urn:retained:fixture" custom:opaque="retain original XMP"/></rdf:RDF></x:xmpmeta>"#)?;
    let root = base.join("catalog");
    let mut catalog = Catalog::open(&root)?;
    let asset = if inspect_original {
        catalog.import(&originals, None, |_| Ok(()))?;
        catalog.browse(0, 1)?[0].id.clone()
    } else {
        // Metadata-only imported fixture: the original has never been inspected.
        // Retain the sidecar packet without inventing a full original digest.
        let asset = "pending-imported-original".to_owned();
        let encoded = |path: &Path| match NativePath::from_path(path) {
            NativePath::UnixBytes(bytes) => bytes,
            NativePath::WindowsWide(units) => units.iter().flat_map(|u| u.to_le_bytes()).collect(),
        };
        let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
        db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?3,'pending')",
            rusqlite::params![asset, encoded(&path), path.to_string_lossy()],
        )?;
        drop(db);
        catalog.record_storage_path(&asset, &NativePath::from_path(&path))?;
        let sidecar = path.with_extension("xmp");
        catalog.retain_metadata(&asset, &photocatalog::catalog_metadata::Source {
            kind: "sidecar".into(), locator: encoded(&sidecar), display: sidecar.to_string_lossy().into_owned(), ambiguous: false,
            provenance: serde_json::json!({"fixture":"retained sidecar; original never inspected"}),
        }, &photocatalog::xmp_packets::inspect_sidecar(&sidecar, &photocatalog::xmp_packets::Limits::default())?)?;
        asset
    };
    let mut registration = photocatalog::catalog_images::ImportImageRequest {
        import_source: "hydration-fixture".into(),
        capture_revision: "1".into(),
        source_table: "Adobe_images".into(),
        source_id: "master".into(),
        input_digest: "fixture-master".into(),
        adapter_version: "fixture-1".into(),
        asset_id: asset,
        claim_reserved_master: false,
        role: photocatalog::catalog_images::ImageRole::Master,
        master: None,
        label: "translated source master".into(),
    };
    let master = catalog.register_import_image(&registration)?.key;
    catalog.save_edit_recipe(
        &master,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 0.5,
            ..RecipeV1::default()
        }),
    )?;
    registration.source_id = "copy".into();
    registration.input_digest = "fixture-copy".into();
    registration.role = photocatalog::catalog_images::ImageRole::Virtual;
    registration.master = Some(master.clone());
    registration.label = "translated virtual copy".into();
    let copy = catalog.register_import_image(&registration)?.key;
    catalog.save_edit_recipe(
        &copy,
        0,
        &Recipe::V1(RecipeV1 {
            exposure_ev: 1.5,
            ..RecipeV1::default()
        }),
    )?;
    drop(catalog);
    let db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
    // Reproduce the exact metadata-only physical state after supported imported
    // master/virtual registration. Logical identities remain immutable.
    db.execute("UPDATE assets SET state='pending',fingerprint=NULL,metadata=NULL,preview_hash=NULL WHERE id=?1",[&master.asset_id])?;
    Ok((root, path, master, copy))
}
fn retained_state(root: &Path) -> Result<serde_json::Value> {
    let db = rusqlite::Connection::open_with_flags(
        root.join("catalog.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut state = std::collections::BTreeMap::new();
    for table in [
        "catalog_images",
        "edit_variants",
        "edit_recipe_nodes",
        "edit_redo_nodes",
        "edit_changes",
        "metadata_assets",
        "metadata_history",
        "metadata_effective",
        "metadata_blobs",
        "metadata_sources",
        "metadata_observations",
        "metadata_packets",
        "metadata_models",
        "metadata_values",
        "metadata_choices",
        "metadata_image_sources",
        "metadata_image_observations",
    ] {
        let mut query = db.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
        let columns = query.column_count();
        let rows = query
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get_ref(i).map(|value| format!("{value:?}")))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        state.insert(table, rows);
    }
    Ok(serde_json::to_value(state)?)
}
fn request_initial_preview(
    b: &Bridge,
    token: &str,
    key: &VariantKey,
    viewport: &str,
    tier: PreviewTier,
) -> Result<PreviewStatus> {
    let Response::Preview(ticket) = call(
        b,
        Request::Preview {
            catalog: token.into(),
            key: key.clone(),
            tier,
            interactive: false,
            viewport: viewport.into(),
            generation: U64(1),
            foreground: true,
        },
    )?
    else {
        bail!("preview reply")
    };
    Ok(ticket)
}
#[test]
fn pending_imported_variants_hydrate_on_demand_without_rewriting_recipes_or_xmp() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (root, path, master, copy) = pending_imported_fixture(temp.path())?;
    let retained = retained_state(&root)?;
    let original = std::fs::read(&path)?;
    let sidecar = std::fs::read(path.with_extension("xmp"))?;
    let b = Bridge::spawn(config(path.parent().unwrap()))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let selected = request_initial_preview(&b, &token, &copy, "selected", PreviewTier::Large)?;
    assert!(matches!(selected.state, PreviewState::Queued));
    assert_eq!(selected.message.as_deref(), Some("preparing original"));
    let thumbnail = request_initial_preview(&b, &token, &master, "master", PreviewTier::Thumbnail)?;
    let selected = preview_ready(&b, &token, selected)?;
    let thumbnail = preview_ready(&b, &token, thumbnail)?;
    let selected_bytes = b
        .preview_bytes(token.clone(), selected.ticket, true)?
        .recv()?;
    let master_bytes = b
        .preview_bytes(token.clone(), thumbnail.ticket, true)?
        .recv()?;
    let selected = image::load_from_memory(selected_bytes.bytes())?.to_rgb8();
    let master_pixels = image::load_from_memory(master_bytes.bytes())?.to_rgb8();
    assert!(selected.get_pixel(0, 0).0[0] > master_pixels.get_pixel(0, 0).0[0]);
    drop(selected_bytes);
    drop(master_bytes);
    b.shutdown();
    assert_eq!(retained_state(&root)?, retained);
    let c = Catalog::open(&root)?;
    let source = c.render_identity(&master.asset_id)?;
    assert_eq!(source.state, "ready");
    assert_eq!(
        source.fingerprint,
        Some(blake3::hash(&original).to_hex().to_string())
    );
    assert_eq!(c.edit_variant(&master)?.revision, 1);
    assert_eq!(c.edit_variant(&copy)?.revision, 1);
    assert_eq!(std::fs::read(&path)?, original);
    assert_eq!(std::fs::read(path.with_extension("xmp"))?, sidecar);
    Ok(())
}

#[test]
fn desktop_relink_unverified_imported_variants_confirm_hydrate_and_undo() -> Result<()> {
    use photocatalog::application::relink as r;
    let temp = tempfile::tempdir()?;
    let (root, path, master, copy) = pending_imported_fixture_with_digest(temp.path(), false)?;
    let custody = |root: &Path| -> Result<serde_json::Value> {
        let mut state = retained_state(root)?;
        // Relink/undo intentionally append location history and advance metadata
        // versions. Retained packets, decisions, observations and recipes stay exact.
        state.as_object_mut().unwrap().retain(|name, _| {
            matches!(
                name.as_str(),
                "edit_variants"
                    | "edit_recipe_nodes"
                    | "edit_redo_nodes"
                    | "edit_changes"
                    | "metadata_blobs"
                    | "metadata_sources"
                    | "metadata_observations"
                    | "metadata_packets"
                    | "metadata_models"
                    | "metadata_values"
                    | "metadata_choices"
            )
        });
        Ok(state)
    };
    let retained = custody(&root)?;
    let original = std::fs::read(&path)?;
    let xmp = std::fs::read(path.with_extension("xmp"))?;
    let relocated = temp.path().join("relocated");
    std::fs::create_dir(&relocated)?;
    let candidate = relocated.join("source.png");
    std::fs::copy(&path, &candidate)?;
    std::fs::copy(path.with_extension("xmp"), candidate.with_extension("xmp"))?;
    let b = Bridge::spawn(config(path.parent().unwrap()))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let relink = |request| -> Result<r::Response> {
        let Response::Relink(response) = call(
            &b,
            Request::Relink {
                catalog: token.clone(),
                request: Box::new(request),
            },
        )?
        else {
            bail!("relink reply")
        };
        Ok(*response)
    };
    let await_plan = |response: r::Response| -> Result<r::Plan> {
        let r::Response::Operation(Some(operation)) = response else {
            bail!("operation reply")
        };
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let r::Response::Operation(Some(s)) = relink(r::Request::Status {
                operation: Some(operation.id.clone()),
            })?
            else {
                bail!("status reply")
            };
            if s.phase == r::Phase::Complete {
                return s.plan.ok_or_else(|| anyhow::anyhow!("missing final plan"));
            }
            ensure!(
                !matches!(s.phase, r::Phase::Failed | r::Phase::Canceled),
                "relink failed: {s:?}"
            );
            ensure!(Instant::now() < until, "relink deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let r::Response::Plan(start) = relink(r::Request::Begin {
        scope: r::Scope::Prefix {
            from: photocatalog::catalog_storage::PathReference::native(path.parent().unwrap()),
            destinations: vec![NativePath::from_path(&relocated)],
        },
    })?
    else {
        bail!("begin reply")
    };
    let reviewed = await_plan(relink(r::Request::Prepare {
        plan: start.id.clone(),
        revision: start.revision,
        batch_rows: U64(1),
    })?)?;
    assert_eq!(reviewed.unverified, I64(1));
    let r::Response::Items { rows, next } = relink(r::Request::Items {
        plan: reviewed.id.clone(),
        revision: reviewed.revision,
        after: I64(0),
        limit: U64(1),
    })?
    else {
        bail!("items")
    };
    assert_eq!(rows.len(), 1);
    assert!(next.is_none());
    assert_eq!(rows[0].identity_basis, "unverified");
    let confirmation = reviewed.confirmation_token.context("confirmation token")?;
    let wrong = relink(r::Request::Confirm {
        plan: reviewed.id.clone(),
        revision: reviewed.revision,
        token: "stale".into(),
        acknowledgement: "no_retained_original_digest".into(),
    })?;
    // Admission is asynchronous; token failure must leave a terminal failed operation.
    assert!(await_plan(wrong).is_err());
    let confirmed = await_plan(relink(r::Request::Confirm {
        plan: reviewed.id,
        revision: reviewed.revision,
        token: confirmation,
        acknowledgement: "no_retained_original_digest".into(),
    })?)?;
    assert_eq!(confirmed.user_confirmed, I64(1));
    let applied = await_plan(relink(r::Request::Apply {
        plan: confirmed.id,
        revision: confirmed.revision,
    })?)?;
    assert_eq!(applied.state, "applied");
    wait_ready(&b)?;
    let preview =
        request_initial_preview(&b, &token, &copy, "relinked-virtual", PreviewTier::Large)?;
    let rendered = preview_ready(&b, &token, preview)?;
    let bytes = b
        .preview_bytes(token.clone(), rendered.ticket, true)?
        .recv()?;
    assert!(image::load_from_memory(bytes.bytes()).is_ok());
    drop(bytes);
    let undone = await_plan(relink(r::Request::Undo {
        plan: applied.id,
        revision: applied.revision,
    })?)?;
    assert_eq!(undone.state, "undone");
    b.shutdown();
    assert_eq!(custody(&root)?, retained);
    let c = Catalog::open(&root)?;
    assert_eq!(
        c.storage_status(
            &master.asset_id,
            &photocatalog::storage_volume::MountSnapshot {
                mounts: vec![],
                complete: false,
                issues: vec![]
            }
        )?
        .current,
        photocatalog::catalog_storage::PathReference::native(&path)
    );
    assert_eq!(c.edit_variant(&master)?.revision, 1);
    assert_eq!(c.edit_variant(&copy)?.revision, 1);
    assert_eq!(std::fs::read(&path)?, original);
    assert_eq!(std::fs::read(path.with_extension("xmp"))?, xmp);
    assert_eq!(std::fs::read(&candidate)?, original);
    assert_eq!(std::fs::read(candidate.with_extension("xmp"))?, xmp);
    Ok(())
}
#[test]
fn restored_pending_preview_keeps_external_job_hold_and_missing_source_is_truthful() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (root, path, master, copy) = pending_imported_fixture(temp.path())?;
    let bundle = temp.path().join("backup");
    let restored = temp.path().join("restored");
    let limits = photocatalog::catalog_backup::Limits::default();
    photocatalog::catalog_backup::backup_catalog(&root, &bundle, &limits, |_| Ok(()))?;
    photocatalog::catalog_backup::restore_catalog(&bundle, &restored, &limits, |_| Ok(()))?;
    let retained = retained_state(&restored)?;
    let b = Bridge::spawn(config(path.parent().unwrap()))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&restored),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    assert!(status(&b)?.jobs_held);
    let preview = request_initial_preview(&b, &token, &copy, "selected", PreviewTier::Large)?;
    preview_ready(&b, &token, preview)?;
    assert!(status(&b)?.jobs_held);
    assert!(
        photocatalog::catalog_backup::restore_status(&restored)?
            .unwrap()
            .jobs_held
    );
    b.shutdown();
    assert_eq!(retained_state(&restored)?, retained);
    std::fs::rename(&path, path.with_extension("moved"))?;
    let b = Bridge::spawn(config(path.parent().unwrap()))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    let token = status(&b)?.catalog.unwrap();
    let mut ticket = request_initial_preview(&b, &token, &master, "missing", PreviewTier::Large)?;
    let until = Instant::now() + Duration::from_secs(10);
    while matches!(ticket.state, PreviewState::Queued) {
        ensure!(Instant::now() < until, "missing source never resolved");
        std::thread::sleep(Duration::from_millis(2));
        let Response::Preview(next) = call(
            &b,
            Request::PreviewStatus {
                catalog: token.clone(),
                ticket: ticket.ticket.clone(),
            },
        )?
        else {
            bail!("status reply")
        };
        ticket = next;
    }
    assert!(matches!(ticket.state, PreviewState::Unavailable));
    assert!(ticket.message.is_some());
    b.shutdown();
    let c = Catalog::open(&root)?;
    assert_eq!(c.render_identity(&master.asset_id)?.state, "pending");
    assert!(c.render_identity(&master.asset_id)?.fingerprint.is_none());
    Ok(())
}

#[test]
fn translated_import_recipes_hydrate_without_losing_import_history() -> Result<()> {
    use photocatalog::{catalog_edits::install_import_recipe, catalog_images::TranslationState};
    let temp = tempfile::tempdir()?;
    let (root, path, master, copy) = pending_imported_fixture(temp.path())?;
    {
        let mut db = rusqlite::Connection::open(root.join("catalog.sqlite3"))?;
        let tx = db.transaction()?;
        for (key, exposure) in [(&master, 0.75), (&copy, 1.75)] {
            let recipe = Recipe::V1(RecipeV1 {
                exposure_ev: exposure,
                ..Default::default()
            })
            .validate()?;
            install_import_recipe(
                &tx,
                key,
                1,
                &recipe,
                &serde_json::json!({"import_source":"hydration-fixture","translation":"supported"}),
                TranslationState::Translated,
            )?;
        }
        tx.commit()?;
    }
    let retained = retained_state(&root)?;
    let original = std::fs::read(&path)?;
    let b = Bridge::spawn(config(path.parent().unwrap()))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    for (key, viewport, tier) in [
        (&copy, "selected", PreviewTier::Large),
        (&master, "master", PreviewTier::Thumbnail),
    ] {
        let ticket = request_initial_preview(&b, &token, key, viewport, tier)?;
        preview_ready(&b, &token, ticket)?;
    }
    b.shutdown();
    assert_eq!(retained_state(&root)?, retained);
    let catalog = Catalog::open(&root)?;
    for key in [&master, &copy] {
        assert_eq!(catalog.image(key)?.translation_state, "translated");
        assert_eq!(catalog.edit_variant(key)?.revision, 2);
    }
    assert_eq!(std::fs::read(&path)?, original);
    assert_eq!(catalog.render_identity(&master.asset_id)?.state, "ready");
    Ok(())
}

#[test]
fn desktop_organization_scopes_changes_to_selected_variant_and_catalog() -> Result<()> {
    use photocatalog::organization::{KeywordKind, Operation};
    let temp = tempfile::tempdir()?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    image::RgbImage::from_pixel(8, 8, image::Rgb([20u8, 40, 80]))
        .save(originals.join("photo.png"))?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    let original = c.edit_variant(&master)?;
    let copy = c.create_edit_variant(&master, original.revision, "Selected copy")?;
    drop(c);
    let b = Bridge::spawn(config(&originals))?;
    call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?;
    wait_ready(&b)?;
    let token = status(&b)?.catalog.unwrap();
    let request = |catalog: &str, request| Request::Organization {
        catalog: catalog.into(),
        request: Box::new(request),
    };
    assert!(
        call(
            &b,
            request(
                "stale",
                organization::Request::CreateCollection {
                    name: "Must not exist".into(),
                }
            )
        )
        .is_err()
    );
    let Response::Organization(response) = call(
        &b,
        request(
            &token,
            organization::Request::Identity {
                key: copy.key.clone(),
            },
        ),
    )?
    else {
        bail!("organization identity response");
    };
    let organization::Response::Identity(identity) = *response else {
        bail!("identity payload");
    };
    call(
        &b,
        request(
            &token,
            organization::Request::Apply {
                key: copy.key.clone(),
                expected_revision: identity.metadata_revision,
                operation: Operation::AddKeyword {
                    kind: KeywordKind::Hierarchical,
                    path: vec!["Travel".into(), "Festival".into()],
                },
            },
        ),
    )?;
    let Response::Organization(response) = call(
        &b,
        request(
            &token,
            organization::Request::Keywords {
                kind: KeywordKind::Hierarchical,
                parent: None,
                after: I64(0),
                limit: 100,
            },
        ),
    )?
    else {
        bail!("keyword response");
    };
    let organization::Response::Keywords(page) = *response else {
        bail!("keyword payload");
    };
    let parent = page.rows.iter().find(|k| k.name == "Travel").unwrap().id;
    let Response::Organization(response) = call(
        &b,
        request(
            &token,
            organization::Request::Keywords {
                kind: KeywordKind::Hierarchical,
                parent: Some(parent),
                after: I64(0),
                limit: 100,
            },
        ),
    )?
    else {
        bail!("child response");
    };
    let organization::Response::Keywords(page) = *response else {
        bail!("child payload");
    };
    let keyword = page.rows.iter().find(|k| k.name == "Festival").unwrap().id;
    wait_ready(&b)?;
    let Response::Images { rows, .. } = call(
        &b,
        Request::Search {
            catalog: token.clone(),
            options: Box::new(browse::Options {
                keyword: Some(keyword),
                ..Default::default()
            }),
            cursor: None,
            limit: 100,
        },
    )?
    else {
        bail!("search response");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, copy.key);
    assert_eq!(variant(&b, &token, &master)?.revision.0, original.revision);
    let Response::Organization(response) = call(
        &b,
        request(
            &token,
            organization::Request::Collections {
                after: String::new(),
                limit: 100,
            },
        ),
    )?
    else {
        bail!("collections response");
    };
    let organization::Response::Collections(page) = *response else {
        bail!("collections payload");
    };
    assert!(page.rows.is_empty());
    b.shutdown();
    Ok(())
}

#[test]
fn desktop_export_explicit_recovery_native_run_and_saved_authority() -> Result<()> {
    use photocatalog::application::exports as ex;
    use photocatalog::image_export::{AlphaPolicy, IntegerDepth, OutputFormat, OutputSize};
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = temp.path().join("originals");
    std::fs::create_dir(&originals)?;
    let path = originals.join("selected.png");
    image::RgbImage::from_pixel(64, 48, image::Rgb([30u8, 70, 110])).save(&path)?;
    let original = std::fs::read(&path)?;
    let root = temp.path().join("catalog");
    let mut c = Catalog::open(&root)?;
    c.import(&originals, None, |_| Ok(()))?;
    let master = VariantKey::master(c.browse(0, 1)?[0].id.clone());
    let selected = c.create_edit_variant(&master, 0, "Selected copy")?.key;
    let revision = c.edit_variant(&selected)?.revision;
    drop(c);
    let b = Bridge::spawn(config(&originals))?;
    let Response::Status(opened) = call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        bail!("open")
    };
    let token = opened.catalog.unwrap();
    wait_ready(&b)?;
    let send = |request| -> Result<ex::Response> {
        let Response::Export(response) = call(
            &b,
            Request::Export {
                catalog: token.clone(),
                request: Box::new(request),
            },
        )?
        else {
            bail!("export envelope")
        };
        Ok(*response)
    };
    let wait = |operation: ex::Operation| -> Result<ex::Operation> {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let ex::Response::Operation(Some(current)) = send(ex::Request::Status {
                operation: Some(operation.id.clone()),
            })?
            else {
                bail!("status")
            };
            if ["complete", "failed", "canceled", "paused"].contains(&current.phase.as_str()) {
                return Ok(current);
            }
            ensure!(
                Instant::now() < until,
                "export operation deadline: {current:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let act = |request| -> Result<ex::Operation> {
        let ex::Response::Operation(Some(operation)) = send(request)? else {
            bail!("operation")
        };
        wait(operation)
    };
    let ex::Response::Options(options) = send(ex::Request::Options)? else {
        bail!("options")
    };
    ensure!(
        options.profile_bytes == U64(16 * 1024 * 1024),
        "profile bound"
    );
    let aliases = act(ex::Request::Paths { limit: U64(512) })?;
    ensure!(aliases.phase == "complete", "{aliases:?}");
    let ex::Response::Job(job) = send(ex::Request::Begin)? else {
        bail!("job")
    };
    let format = OutputFormat::Png {
        depth: IntegerDepth::Sixteen,
    };
    let names = act(ex::Request::Destinations {
        directory: NativePath::from_path(temp.path()),
        targets: vec![ex::TargetKey {
            key: selected.clone(),
            expected_revision: I64(revision),
        }],
        format,
        naming: ex::Naming {
            prefix: "export-".into(),
            suffix: String::new(),
            variant_suffix: true,
            sequence_start: None,
        },
    })?;
    let Some(ex::ResultValue::Destinations {
        token: names_token,
        total,
    }) = names.result.map(|value| *value)
    else {
        bail!(
            "destination result missing (phase {}, error {:?})",
            names.phase,
            names.error
        )
    };
    ensure!(total == U64(1), "destination count");
    let ex::Response::Destinations { rows, next, .. } = send(ex::Request::DestinationRows {
        token: names_token.clone(),
        after: U64(0),
        limit: U64(1),
    })?
    else {
        bail!("destination rows")
    };
    ensure!(
        next.is_none() && rows[0].name.variant_label == "Selected copy",
        "named selection"
    );
    let destination = rows[0]
        .destination
        .clone()
        .context("composed destination")?;
    send(ex::Request::ResultRelease { token: names_token })?;
    let ex::Response::Operation(Some(admitted)) = send(ex::Request::Append {
        job: job.id.clone(),
        expected_total: I64(0),
        target: ex::Target {
            key: selected.clone(),
            expected_revision: I64(revision),
            destination: destination.clone(),
            overwrite: false,
            metadata: ex::Metadata::Omit,
        },
        output: ex::Output {
            size: OutputSize::Original,
            format,
            profile: ex::Profile::Srgb,
            alpha: AlphaPolicy::Preserve,
        },
        budgets: None,
    })?
    else {
        bail!("append admission")
    };
    ensure!(
        admitted.job.as_ref().is_some_and(|j| j.id == job.id),
        "first operation must identify job"
    );
    let appended = wait(admitted)?;
    ensure!(appended.phase == "complete", "{appended:?}");
    send(ex::Request::Seal {
        job: job.id.clone(),
        expected_total: I64(1),
    })?;
    let run = || ex::Request::Run {
        job: job.id.clone(),
        limits: None,
        max_items: U64(1),
        max_seconds: U64(30),
    };
    let failed = act(run())?;
    ensure!(
        failed.phase == "failed"
            && failed
                .error
                .as_deref()
                .is_some_and(|s| s.contains("explicit export recovery")),
        "Run cannot silently recover: {failed:?}"
    );
    ensure!(
        !destination.to_path()?.exists(),
        "failed admission must not publish"
    );
    let recovered = act(ex::Request::Recover {
        directories: U64(32),
        limits: None,
    })?;
    ensure!(recovered.phase == "complete", "{recovered:?}");
    let completed = act(run())?;
    ensure!(
        completed.phase == "complete"
            && completed
                .job
                .as_ref()
                .is_some_and(|j| j.state == "complete"),
        "{completed:?}"
    );
    let exported = image::open(destination.to_path()?)?;
    ensure!(
        exported.width() == 64 && exported.height() == 48,
        "native output dimensions"
    );
    let ex::Response::Plan(plan) = send(ex::Request::Plan {
        job: job.id.clone(),
        sequence: I64(1),
    })?
    else {
        bail!("plan")
    };
    ensure!(
        plan.identity.key == selected && plan.item.state == "published",
        "selected frozen identity"
    );
    let restored = act(ex::Request::Restore {
        job: job.id.clone(),
        sequence: I64(1),
        authority: plan.authority.clone(),
    })?;
    ensure!(
        matches!(restored.result.as_deref(), Some(ex::ResultValue::Receipt { receipt, .. }) if receipt.state == "published"),
        "installed publication remains published"
    );
    ensure!(std::fs::read(&path)? == original, "source original changed");
    call(
        &b,
        Request::Close {
            catalog: token.clone(),
        },
    )?;
    let Response::Status(reopened) = call(
        &b,
        Request::OpenExisting {
            path: NativePath::from_path(&root),
        },
    )?
    else {
        bail!("reopen")
    };
    let new_token = reopened.catalog.unwrap();
    ensure!(new_token != token, "session replacement");
    let Response::Export(response) = call(
        &b,
        Request::Export {
            catalog: new_token.clone(),
            request: Box::new(ex::Request::Plan {
                job: job.id,
                sequence: I64(1),
            }),
        },
    )?
    else {
        bail!("saved plan")
    };
    let ex::Response::Plan(saved) = *response else {
        bail!("saved plan kind")
    };
    ensure!(
        saved.authority == plan.authority,
        "exact persisted authority changed"
    );
    call(&b, Request::Close { catalog: new_token })?;
    b.shutdown();
    Ok(())
}
