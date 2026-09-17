use super::*;

fn config(root: &Path) -> StoreConfig {
    StoreConfig {
        manifest_root: root.join("manifest"),
        thumbnail_root: root.join("thumb"),
        large_root: root.join("large"),
        layout: Layout::Flat,
        thumbnail_bytes: 4096,
        large_bytes: 4096,
    }
}
fn key() -> PreviewKey {
    PreviewKey {
        image_pixel_generation: None,
        asset_id: "asset".into(),
        variant_id: "master".into(),
        generation: 1,
        fingerprint: "a".repeat(64),
        edit_revision: 1,
        renderer_version: "test-1".into(),
        preparation_version: PREPARATION_VERSION.into(),
        tier: Tier::Thumbnail,
        edge: 256,
        encoding: CodecSettings {
            codec: super::super::super::Codec::Jpeg,
            quality: 65,
        },
    }
}
#[test]
fn native_codec_is_exact_and_rejects_invalid_or_oversized_authority() -> Result<()> {
    let root = tempfile::tempdir()?;
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'x', 255])
    };
    #[cfg(windows)]
    let odd = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[120, 0xd800])
    };
    let path = root.path().join(odd);
    let encoded = encode_path(&path)?;
    assert_eq!(decode_path(ValueRef::Blob(&encoded))?, path);
    let legacy = root.path().join("literal-{\"version\":1}");
    assert_eq!(
        decode_path(ValueRef::Text(legacy.to_str().unwrap().as_bytes()))?,
        legacy
    );
    for bytes in [
        b"{}".to_vec(),
        b"null".to_vec(),
        vec![b' '; MAX_PATH_BYTES + 1],
        serde_json::to_vec(
            &serde_json::json!({"version": 2, "path": NativePath::from_path(root.path())}),
        )?,
        serde_json::to_vec(
            &serde_json::json!({"version": 1, "path": {"encoding":"UnixBytes", "units":[47,0]}}),
        )?,
        serde_json::to_vec(
            &serde_json::json!({"version": 1, "path": {"encoding":"WindowsWide", "units":[67,58,92,0]}}),
        )?,
    ] {
        assert!(decode_path(ValueRef::Blob(&bytes)).is_err());
    }
    #[cfg(unix)]
    let foreign = NativePath::WindowsWide(vec![67, 58, 92, 120]);
    #[cfg(windows)]
    let foreign = NativePath::UnixBytes(b"/x".to_vec());
    assert!(
        decode_path(ValueRef::Blob(&serde_json::to_vec(&StoredPath {
            version: 1,
            path: foreign
        })?))
        .is_err()
    );
    assert!(decode_path(ValueRef::Text(&vec![b'a'; MAX_PATH_BYTES + 1])).is_err());
    assert!(decode_path(ValueRef::Text(b"relative")).is_err());
    assert!(decode_path(ValueRef::Integer(1)).is_err());
    Ok(())
}
fn relocation_roundtrip(root: &Path, manifest: Option<&Path>, legacy: bool) -> Result<()> {
    let mut cfg = config(root);
    if let Some(manifest) = manifest {
        cfg.manifest_root = manifest.to_owned();
    }
    let mut store = PreviewStore::open(cfg.clone(), &[])?;
    let key = key();
    store.desire(&key, || Ok(true))?;
    store.publish(&key, b"retained preview", |attach| attach())?;
    let target = root.join("moved");
    store.begin_relocation(Tier::Thumbnail, &target, &[])?;
    if legacy {
        for tier in [Tier::Thumbnail, Tier::Large] {
            store.db.execute(
                "UPDATE locations SET path=?1 WHERE tier=?2",
                params![store.root(tier).to_str().unwrap(), tier.name()],
            )?;
        }
        store.db.execute(
            "UPDATE relocations SET source=?1,target=?2",
            params![
                store.root(Tier::Thumbnail).to_str().unwrap(),
                fs::canonicalize(&target)?.to_str().unwrap()
            ],
        )?;
        store.db.execute_batch("PRAGMA user_version=4")?;
    }
    drop(store);
    let mut store = PreviewStore::open(PreviewStore::current_configuration(cfg.clone())?, &[])?;
    assert_eq!(
        store
            .db
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        6
    );
    if legacy {
        assert_eq!(
            store.db.query_row(
                "SELECT typeof(source)||':'||typeof(target) FROM relocations",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "text:text"
        );
        assert_eq!(
            store.db.query_row(
                "SELECT count(*) FROM locations WHERE typeof(path)='text'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            2
        );
    }
    assert!(
        store
            .ensure_original_separate(&target.join("original.png"))
            .is_err()
    );
    assert_eq!(store.read(&key, false)?.unwrap().bytes, b"retained preview");
    assert_eq!(
        store.relocation_step(Tier::Thumbnail, 1, 4096)?.phase,
        "copy"
    );
    assert_eq!(
        store.relocation_step(Tier::Thumbnail, 1, 4096)?.phase,
        "cleanup"
    );
    drop(store);
    let mut store = PreviewStore::open(PreviewStore::current_configuration(cfg)?, &[])?;
    assert_eq!(store.root(Tier::Thumbnail), fs::canonicalize(&target)?);
    assert!(
        store
            .ensure_original_separate(&root.join("thumb/original.png"))
            .is_err()
    );
    while !store.relocation_step(Tier::Thumbnail, 1, 4096)?.complete {}
    assert_eq!(store.read(&key, false)?.unwrap().bytes, b"retained preview");
    assert_eq!(store.usage()?.thumbnail_bytes, 16);
    Ok(())
}
#[test]
fn legacy_and_native_locations_resume_both_phases_without_losing_objects() -> Result<()> {
    for legacy in [true, false] {
        let root = tempfile::tempdir()?;
        relocation_roundtrip(root.path(), None, legacy)?;
    }
    Ok(())
}
#[test]
fn physical_native_cache_paths_relocate_and_reopen() -> Result<()> {
    let temp = tempfile::tempdir()?;
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![b'c', 255])
    };
    #[cfg(windows)]
    let odd = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[99, 0xd800])
    };
    let root = temp.path().join(odd);
    if let Err(error) = fs::create_dir(&root) {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(92) {
            eprintln!(
                "physical native cache path rejected at filesystem admission: EILSEQ92; codec coverage is unconditional"
            );
            return Ok(());
        }
        return Err(error.into());
    }
    // SQLite's Windows API rejects unpaired surrogates in its database filename.
    // Keep that database representable while exercising actual native thumbnail,
    // large-cache and relocation paths (including both restart phases).
    let manifest = cfg!(windows).then(|| temp.path().join("manifest"));
    relocation_roundtrip(&root, manifest.as_deref(), false)
}

#[cfg(windows)]
#[test]
fn unrepresentable_manifest_path_fails_before_creating_cache_roots() -> Result<()> {
    use std::os::windows::ffi::OsStringExt;
    let temp = tempfile::tempdir()?;
    let root = temp
        .path()
        .join(std::ffi::OsString::from_wide(&[99, 0xd800]));
    let error = PreviewStore::open(config(&root), &[]).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("manifest path must be valid Unicode")
    );
    assert!(!root.exists());
    Ok(())
}
#[test]
fn malformed_or_future_cache_authority_leaves_schema_and_rows_unchanged() -> Result<()> {
    let root = tempfile::tempdir()?;
    let cfg = config(root.path());
    let store = PreviewStore::open(cfg.clone(), &[])?;
    store.db.execute_batch(
        "PRAGMA user_version=4; UPDATE locations SET path=x'7b7d' WHERE tier='thumbnail'",
    )?;
    drop(store);
    assert!(PreviewStore::current_configuration(cfg.clone()).is_err());
    assert!(PreviewStore::open(cfg.clone(), &[]).is_err());
    let db = Connection::open(cfg.manifest_root.join("previews.sqlite3"))?;
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT path FROM locations WHERE tier='thumbnail'",
            [],
            |r| r.get::<_, Vec<u8>>(0)
        )?,
        b"{}"
    );
    db.execute_batch("UPDATE locations SET path=zeroblob(2097152) WHERE tier='thumbnail'")?;
    assert!(PreviewStore::current_configuration(cfg.clone()).is_err());
    assert!(PreviewStore::open(cfg.clone(), &[]).is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        4
    );
    // The same SQL admission used by path readers returns no oversized body.
    let oversized: Option<Vec<u8>> = db.query_row("SELECT CASE WHEN length(CAST(path AS BLOB))<=1048576 THEN path ELSE NULL END FROM locations WHERE tier='thumbnail'", [], |r| r.get(0))?;
    assert!(oversized.is_none());
    db.execute_batch("PRAGMA user_version=6")?;
    assert!(PreviewStore::current_configuration(cfg.clone()).is_err());
    assert!(PreviewStore::open(cfg, &[]).is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))?,
        6
    );
    Ok(())
}
