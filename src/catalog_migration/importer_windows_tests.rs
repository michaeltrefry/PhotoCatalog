use super::*;

fn inspection_paths(fixture: &mut ImportFixture, paths: &[NativePath]) {
    let revisions: Vec<_> = fixture
        .inspection
        .seal
        .selected
        .iter()
        .map(|s| s.revision.clone())
        .collect();
    fixture.inspection.edit(|db| {
        for (index, revision) in revisions.iter().enumerate() {
            for key in [10, 11] {
                let path = &paths[if key == 10 { 0 } else { index + 1 }];
                db.execute(
                    "UPDATE paths SET inspection_path=? WHERE revision=? AND source_id=?",
                    params![
                        serde_json::to_string(path).unwrap(),
                        revision,
                        source_id("AgLibraryFile", key)
                    ],
                )
                .unwrap();
            }
        }
    });
}

#[test]
fn direct_import_then_plain_windows_lightroom_reuses_originals_through_resume() -> Result<()> {
    let mut fixture = ImportFixture::new(false)?;
    for path in &fixture.paths {
        image::RgbImage::from_pixel(8, 8, image::Rgb([45, 90, 180])).save(path.to_path()?)?;
    }
    let originals: Vec<_> = fixture
        .paths
        .iter()
        .map(|p| Ok(fs::read(p.to_path()?)?))
        .collect::<Result<_>>()?;
    let plain: Vec<_> = fixture
        .paths
        .iter()
        .map(|path| {
            let NativePath::WindowsWide(units) = path else {
                panic!("Windows fixture")
            };
            assert_eq!(&units[..4], &[92, 92, 63, 92]);
            NativePath::WindowsWide(units[4..].to_vec())
        })
        .collect();
    inspection_paths(&mut fixture, &plain);
    let source_before = fs::read(&fixture.inspection.path)?;
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    catalog.import(fixture._root.path().join("originals"), None, |_| Ok(()))?;
    assert_eq!(count(&catalog, "SELECT count(*) FROM assets")?, 3);
    let locations: Vec<(String, Vec<u8>)> = catalog
        .db
        .prepare("SELECT id,location FROM assets ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let user_copy = catalog
        .create_edit_variant(&VariantKey::master(&locations[0].0), 0, "User copy")?
        .key;
    let bindings_before: Vec<(String, String, Option<String>)> = catalog
        .db
        .prepare("SELECT asset_id,native_path,file_key FROM storage_bindings ORDER BY asset_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Files)?;
    worker.step(&mut catalog, &|| false)?;
    drop(worker);
    drop(catalog);
    let mut catalog = fixture.open()?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Complete)?;
    assert_eq!(count(&catalog, "SELECT count(*) FROM assets")?, 3);
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM migration_originals")?,
        4
    );
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM migration_originals WHERE created=1"
        )?,
        0
    );
    assert_eq!(count(&catalog, "SELECT count(*) FROM catalog_images")?, 10);
    assert!(catalog.edit_variant(&user_copy).is_ok());
    let bindings_after: Vec<(String, String, Option<String>)> = catalog
        .db
        .prepare("SELECT asset_id,native_path,file_key FROM storage_bindings ORDER BY asset_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(bindings_after, bindings_before);
    for (asset, location) in locations {
        assert_eq!(
            catalog
                .db
                .query_row("SELECT location FROM assets WHERE id=?", [&asset], |r| r
                    .get::<_, Vec<u8>>(0))?,
            location
        );
    }
    let first = fixture.key(&catalog, 0, 20)?;
    let later = fixture.key(&catalog, 1, 20)?;
    assert_eq!(first.asset_id, later.asset_id);
    assert_ne!(first.variant_id, later.variant_id);
    let counts = (
        count(&catalog, "SELECT count(*) FROM migration_run_items")?,
        count(&catalog, "SELECT count(*) FROM image_import_map")?,
    );
    worker.step(&mut catalog, &|| false)?;
    assert_eq!(
        counts,
        (
            count(&catalog, "SELECT count(*) FROM migration_run_items")?,
            count(&catalog, "SELECT count(*) FROM image_import_map")?
        )
    );
    assert_eq!(fs::read(&fixture.inspection.path)?, source_before);
    for (path, bytes) in fixture.paths.iter().zip(originals) {
        assert_eq!(fs::read(path.to_path()?)?, bytes);
    }
    Ok(())
}

#[test]
fn offline_unc_prefix_reuse_keeps_original_registration_authority() -> Result<()> {
    let mut fixture = ImportFixture::new(false)?;
    let plain: Vec<_> = (0..3)
        .map(|i| {
            NativePath::WindowsWide(
                format!(r"\\offline-server\share\photo{i}.jpg")
                    .encode_utf16()
                    .collect(),
            )
        })
        .collect();
    inspection_paths(&mut fixture, &plain);
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    for i in 0..3 {
        let stored = NativePath::WindowsWide(
            format!(r"\\?\UNC\offline-server\share\photo{i}.jpg")
                .encode_utf16()
                .collect(),
        );
        let id = format!("offline-original-{i}");
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?,?,?,'pending')",
            params![
                id,
                crate::catalog_storage::encoded_bytes(&stored),
                "offline original"
            ],
        )?;
        crate::catalog_storage::record_storage_path(&catalog.db, &id, &stored)?;
    }
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Complete)?;
    assert_eq!(count(&catalog, "SELECT count(*) FROM assets")?, 3);
    assert_eq!(
        count(
            &catalog,
            "SELECT count(*) FROM migration_originals WHERE created=1"
        )?,
        0
    );
    assert_eq!(count(&catalog, "SELECT count(*) FROM catalog_images")?, 9);
    Ok(())
}

#[test]
fn ambiguous_prefix_originals_fail_before_registration() -> Result<()> {
    let mut fixture = ImportFixture::new(false)?;
    let plain: Vec<_> = (0..3)
        .map(|i| {
            NativePath::WindowsWide(
                format!(r"C:\synthetic\photo{i}.jpg")
                    .encode_utf16()
                    .collect(),
            )
        })
        .collect();
    inspection_paths(&mut fixture, &plain);
    let source = fixture.inspection.open();
    let mut catalog = fixture.open()?;
    for (id, spelling) in [
        ("ordinary", r"C:\synthetic\photo0.jpg"),
        ("verbatim", r"\\?\C:\synthetic\photo0.jpg"),
    ] {
        let path = NativePath::WindowsWide(spelling.encode_utf16().collect());
        catalog.db.execute(
            "INSERT INTO assets(id,location,path_display,state) VALUES(?,?,?,'pending')",
            params![id, crate::catalog_storage::encoded_bytes(&path), spelling],
        )?;
        crate::catalog_storage::record_storage_path(&catalog.db, id, &path)?;
    }
    let run = catalog.begin_selected_import(&source, APPROVAL, &fixture.policy)?;
    let mut worker = Worker::new(&source, &run.id, ImportFixture::limits())?;
    drive(&mut worker, &mut catalog, Stage::Files)?;
    let error = worker
        .step(&mut catalog, &|| false)
        .expect_err("ambiguous original must fail");
    assert!(format!("{error:#}").contains("Ambiguous Windows original path"));
    assert_eq!(count(&catalog, "SELECT count(*) FROM assets")?, 2);
    assert_eq!(
        count(&catalog, "SELECT count(*) FROM migration_originals")?,
        0
    );
    Ok(())
}
