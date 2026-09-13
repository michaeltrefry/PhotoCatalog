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
