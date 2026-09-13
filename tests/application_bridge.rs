use anyhow::{Result, bail, ensure};
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
