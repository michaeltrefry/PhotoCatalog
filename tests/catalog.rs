use anyhow::Result;
use photocatalog::{Catalog, ImportEvent};
use std::{fs, path::Path};
use tempfile::TempDir;
fn jpeg(path: &Path, color: u8) {
    let image = image::RgbImage::from_fn(96, 64, |x, y| image::Rgb([x as u8, y as u8, color]));
    image.save(path).unwrap();
}
// Public-safe miniature TIFF/CR2 container with an ordinary JPEG at a declared IFD0 strip.
// This validates the parser contract; it is not evidence for a real camera's RAW format.
fn synthetic_cr2(path: &Path) {
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut encoded)
        .encode_image(&image::RgbImage::from_pixel(48, 32, image::Rgb([2, 4, 8])))
        .unwrap();
    let tags = [
        (256, 4, 48),
        (257, 4, 32),
        (259, 3, 6),
        (273, 4, 94),
        (279, 4, encoded.len() as u32),
        (274, 3, 6),
    ];
    let mut data = b"II*\0\x10\0\0\0CR\x02\0\0\0\0\0".to_vec();
    data.extend_from_slice(&(tags.len() as u16).to_le_bytes());
    for (tag, kind, value) in tags {
        data.extend_from_slice(&(tag as u16).to_le_bytes());
        data.extend_from_slice(&(kind as u16).to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(data.len(), 94);
    data.extend_from_slice(&encoded);
    fs::write(path, data).unwrap();
}
fn setup() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("originals ü 日本語");
    fs::create_dir(&src).unwrap();
    let db = tmp.path().join("catalog");
    (tmp, src, db)
}
#[test]
fn stable_identity_restart_pagination_and_offline_preview() -> Result<()> {
    let (_tmp, src, db) = setup();
    jpeg(&src.join("a.jpg"), 3);
    synthetic_cr2(&src.join("b.CR2"));
    let mut catalog = Catalog::open(&db)?;
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 2);
    let first = catalog.browse(0, 1)?.remove(0);
    let second = catalog.browse(first.sequence, 1)?.remove(0);
    assert_ne!(first.id, second.id);
    assert!(catalog.browse(second.sequence, 1)?.is_empty());
    let raw = catalog
        .browse(0, 10)?
        .into_iter()
        .find(|a| a.metadata.as_ref().unwrap().format == "CR2")
        .unwrap();
    let metadata = raw.metadata.as_ref().unwrap();
    assert_eq!(
        (metadata.width, metadata.height, metadata.orientation),
        (48, 32, 6)
    );
    let thumbnail = image::load_from_memory(&catalog.preview(&raw.id)?)?;
    assert_eq!((thumbnail.width(), thumbnail.height()), (32, 48));
    drop(catalog);
    let mut catalog = Catalog::open(&db)?;
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.unchanged, 2);
    assert_eq!(catalog.browse(0, 1)?[0].id, first.id);
    fs::rename(&src, src.with_extension("offline"))?;
    assert!(!catalog.preview(&first.id)?.is_empty());
    assert_eq!(catalog.get(&second.id)?.id, second.id);
    assert!(catalog.browse(0, 1001).is_err());
    Ok(())
}
#[test]
fn interruptions_at_each_durability_boundary_resume() -> Result<()> {
    for boundary in [
        ImportEvent::Reserved,
        ImportEvent::PreviewPublished,
        ImportEvent::Committed,
    ] {
        let (_tmp, src, db) = setup();
        jpeg(&src.join("a.jpg"), 1);
        let mut catalog = Catalog::open(&db)?;
        assert!(
            catalog
                .import(&src, None, |event| {
                    if event == boundary {
                        anyhow::bail!("controlled cancellation");
                    }
                    Ok(())
                })
                .is_err()
        );
        let original = catalog.browse(0, 10)?.remove(0);
        if boundary != ImportEvent::Committed {
            assert_eq!(original.state, "pending");
            assert!(catalog.preview(&original.id).is_err());
        }
        drop(catalog);
        let mut catalog = Catalog::open(&db)?;
        let report = catalog.import(&src, None, |_| Ok(()))?;
        assert_eq!(report.imported + report.unchanged, 1);
        let records = catalog.browse(0, 10)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, original.id);
        assert!(catalog.preview(&original.id).is_ok());
    }
    Ok(())
}
#[test]
fn failed_and_changed_sources_retry_without_duplicate_identity() -> Result<()> {
    let (_tmp, src, db) = setup();
    let path = src.join("bad.jpg");
    fs::write(&path, b"not a JPEG")?;
    let mut catalog = Catalog::open(&db)?;
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.failed, 1);
    let id = catalog.browse(0, 10)?[0].id.clone();
    assert!(catalog.preview(&id).is_err());
    jpeg(&path, 1);
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 1);
    let before = catalog.preview(&id)?;
    jpeg(&path, 250);
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 1);
    assert_ne!(catalog.preview(&id)?, before);
    assert_eq!(catalog.browse(0, 10)?.len(), 1);
    Ok(())
}
#[test]
fn changing_source_during_import_never_publishes_ready() -> Result<()> {
    let (_tmp, src, db) = setup();
    let path = src.join("a.jpg");
    jpeg(&path, 1);
    let mut catalog = Catalog::open(&db)?;
    let report = catalog.import(&src, None, |event| {
        if event == ImportEvent::Reserved {
            jpeg(&path, 240);
        }
        Ok(())
    })?;
    assert_eq!(report.failed, 1);
    let asset = catalog.browse(0, 10)?.remove(0);
    assert_eq!(asset.state, "failed");
    assert!(catalog.preview(&asset.id).is_err());
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 1);
    Ok(())
}
#[test]
fn corrupted_or_missing_previews_are_not_served_and_rebuilt() -> Result<()> {
    let (_tmp, src, db) = setup();
    jpeg(&src.join("a.jpg"), 1);
    let mut catalog = Catalog::open(&db)?;
    catalog.import(&src, None, |_| Ok(()))?;
    let id = catalog.browse(0, 10)?[0].id.clone();
    let path = fs::read_dir(db.join("previews"))?.next().unwrap()?.path();
    fs::write(&path, b"partial")?;
    assert!(catalog.preview(&id).is_err());
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 1);
    assert!(catalog.preview(&id).is_ok());
    fs::remove_file(path)?;
    assert!(catalog.preview(&id).is_err());
    assert_eq!(catalog.import(&src, None, |_| Ok(()))?.imported, 1);
    Ok(())
}
#[test]
fn malformed_tiff_offsets_fail_boundedly() -> Result<()> {
    let (_tmp, src, db) = setup();
    fs::write(
        src.join("bad.CR2"),
        b"II*\0\xff\xff\xff\xffCR\x02\0\0\0\0\0",
    )?;
    let mut catalog = Catalog::open(db)?;
    assert_eq!(catalog.import(src, None, |_| Ok(()))?.failed, 1);
    Ok(())
}
#[test]
fn newer_schema_is_rejected_without_modification() -> Result<()> {
    let (_tmp, _src, db) = setup();
    drop(Catalog::open(&db)?);
    let connection = rusqlite::Connection::open(db.join("catalog.sqlite3"))?;
    connection.pragma_update(None, "user_version", 999)?;
    drop(connection);
    assert!(Catalog::open(&db).is_err());
    let connection = rusqlite::Connection::open(db.join("catalog.sqlite3"))?;
    assert_eq!(
        connection.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        999
    );
    Ok(())
}
#[cfg(target_os = "linux")]
#[test]
fn symlinks_not_followed_and_non_utf8_locations_are_distinct() -> Result<()> {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let (_tmp, src, db) = setup();
    let external = src.with_extension("external");
    fs::create_dir(&external)?;
    jpeg(&external.join("secret.jpg"), 1);
    symlink(&external, src.join("linked"))?;
    for n in [0xfe, 0xff] {
        let name = std::ffi::OsString::from_vec(vec![n, b'.', b'j', b'p', b'g']);
        jpeg(&src.join(name), n);
    }
    let mut catalog = Catalog::open(db)?;
    assert_eq!(catalog.import(src, None, |_| Ok(()))?.imported, 2);
    assert_eq!(catalog.browse(0, 10)?.len(), 2);
    Ok(())
}
#[test]
fn cli_runs_in_separate_processes_and_never_overwrites_output() -> Result<()> {
    let (_tmp, src, db) = setup();
    jpeg(&src.join("a.jpg"), 1);
    synthetic_cr2(&src.join("b.CR2"));
    let binary = assert_cmd::cargo::cargo_bin!("photocatalog");
    assert_cmd::Command::new(binary)
        .arg("--catalog")
        .arg(&db)
        .arg("import")
        .arg(&src)
        .assert()
        .success();
    let output = assert_cmd::Command::new(binary)
        .arg("--catalog")
        .arg(&db)
        .args(["browse", "--limit", "1"])
        .output()?;
    assert!(output.status.success());
    let assets: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let id = assets[0]["id"].as_str().unwrap();
    let destination = db.with_extension("preview.jpg");
    fs::rename(&src, src.with_extension("offline"))?;
    assert_cmd::Command::new(binary)
        .arg("--catalog")
        .arg(&db)
        .args(["preview", id])
        .arg(&destination)
        .assert()
        .success();
    assert_cmd::Command::new(binary)
        .arg("--catalog")
        .arg(&db)
        .args(["preview", id])
        .arg(&destination)
        .assert()
        .failure();
    assert!(image::open(destination).is_ok());
    Ok(())
}
#[test]
fn importer_lock_blocks_second_writer_but_allows_browsing() -> Result<()> {
    let (_tmp, src, db) = setup();
    jpeg(&src.join("a.jpg"), 1);
    let mut first = Catalog::open(&db)?;
    let mut second = Catalog::open(&db)?;
    first.import(&src, None, |event| {
        if event == ImportEvent::Reserved {
            assert!(second.import(&src, None, |_| Ok(())).is_err());
            assert_eq!(second.browse(0, 10)?.len(), 1);
        }
        Ok(())
    })?;
    Ok(())
}
#[test]
fn crash_worker() {
    let Ok(db) = std::env::var("PHOTOCATALOG_TEST_CRASH_DB") else {
        return;
    };
    let src = std::env::var("PHOTOCATALOG_TEST_CRASH_SRC").unwrap();
    let mut catalog = Catalog::open(db).unwrap();
    catalog
        .import(src, None, |event| {
            if event == ImportEvent::PreviewPublished {
                std::process::exit(91);
            }
            Ok(())
        })
        .unwrap();
}
#[test]
fn process_death_after_preview_publication_resumes() -> Result<()> {
    let (_tmp, src, db) = setup();
    jpeg(&src.join("a.jpg"), 1);
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "crash_worker", "--nocapture"])
        .env("PHOTOCATALOG_TEST_CRASH_DB", &db)
        .env("PHOTOCATALOG_TEST_CRASH_SRC", &src)
        .status()?;
    assert_eq!(status.code(), Some(91));
    let mut catalog = Catalog::open(&db)?;
    let id = catalog.browse(0, 10)?[0].id.clone();
    assert!(catalog.preview(&id).is_err());
    assert_eq!(catalog.import(src, None, |_| Ok(()))?.imported, 1);
    assert_eq!(catalog.browse(0, 10)?.len(), 1);
    assert!(catalog.preview(&id).is_ok());
    Ok(())
}

#[test]
fn unrelated_database_is_rejected_without_adoption() -> Result<()> {
    let (_tmp, _src, db) = setup();
    fs::create_dir(&db)?;
    let connection = rusqlite::Connection::open(db.join("catalog.sqlite3"))?;
    connection.execute("CREATE TABLE unrelated (id INTEGER)", [])?;
    drop(connection);
    assert!(Catalog::open(&db).is_err());
    let connection = rusqlite::Connection::open(db.join("catalog.sqlite3"))?;
    assert_eq!(
        connection.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[cfg(unix)]
#[test]
fn directory_and_file_symlinks_are_not_imported() -> Result<()> {
    let (_tmp, src, db) = setup();
    let external = src.with_extension("external");
    fs::create_dir(&external)?;
    jpeg(&external.join("external.jpg"), 1);
    std::os::unix::fs::symlink(&external, src.join("linked"))?;
    std::os::unix::fs::symlink(external.join("external.jpg"), src.join("linked.jpg"))?;
    let mut catalog = Catalog::open(db)?;
    assert_eq!(catalog.import(src, None, |_| Ok(()))?.imported, 0);
    Ok(())
}
