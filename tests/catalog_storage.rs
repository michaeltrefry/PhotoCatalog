use anyhow::{Result, ensure};
use photocatalog::{
    Catalog,
    catalog_storage::{PathReference, RelinkBoundary, RelinkScope},
    storage_volume::{
        IdentityScheme, LocationState, MountSnapshot, MountedVolume, NativePath,
        PersistentVolumeId, VolumeLocation,
    },
};
use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
};
fn photo(path: &Path, color: u8) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    image::RgbImage::from_pixel(8, 8, image::Rgb([color, 40, 60]))
        .save(path)
        .unwrap();
}
fn prefix(from: &Path, to: &Path) -> RelinkScope {
    RelinkScope::Prefix {
        from: PathReference::native(from),
        destinations: vec![NativePath::from_path(to)],
    }
}
fn prepared(cat: &mut Catalog, scope: RelinkScope) -> Result<String> {
    let p = cat.begin_relink(scope)?;
    while cat.relink_plan(&p.id)?.state == "preparing" {
        cat.prepare_relink_batch(&p.id, 1)?;
    }
    Ok(p.id)
}
fn db(root: &Path) -> Result<Connection> {
    Ok(Connection::open(root.join("catalog.sqlite3"))?)
}
fn paths(cat: &Catalog) -> Result<Vec<(String, String)>> {
    Ok(cat
        .browse(0, 100)?
        .into_iter()
        .map(|a| (a.id, a.original_path))
        .collect())
}
fn setup() -> Result<(tempfile::TempDir, PathBuf, PathBuf, Catalog)> {
    let temp = tempfile::Builder::new().tempdir_in(std::env::temp_dir().canonicalize()?)?;
    let originals = temp.path().join("originals");
    fs::create_dir(&originals)?;
    let root = temp.path().join("catalog");
    let cat = Catalog::open(&root)?;
    Ok((temp, originals, root, cat))
}
#[test]
fn missing_tree_relink_restart_sidecar_sources_and_undo_are_atomic() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("nested/雪.jpg"), 20);
    photo(&old.join("other.jpg"), 80);
    fs::write(old.join("nested/雪.xmp"),br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="original-subject" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF>"#)?;
    cat.import(&old, None, |_| Ok(()))?;
    let before = paths(&cat)?;
    let source_count: i64 =
        db(&root)?.query_row("SELECT COUNT(*) FROM metadata_sources", [], |r| r.get(0))?;
    let provenance: Vec<String> = db(&root)?
        .prepare("SELECT provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let new = temp.path().join("upgraded RAID");
    fs::rename(&old, &new)?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    ensure!(
        cat.relink_plan(&p)?.matched == 2 && cat.relink_plan(&p)?.unresolved_sources == 0,
        "plan: {:?}; items: {:?}",
        cat.relink_plan(&p)?,
        cat.relink_items(&p, 0, 10)?
    );
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    let first = cat.relink_items(&p, 0, 1)?;
    ensure!(first.len() == 1);
    ensure!(cat.relink_items(&p, first[0].sequence, 1)?.len() == 1);
    cat.apply_relink(&p)?;
    let after = paths(&cat)?;
    ensure!(before.iter().map(|v| &v.0).eq(after.iter().map(|v| &v.0)));
    for (id, path) in &after {
        ensure!(Path::new(path).starts_with(&new));
        ensure!(!cat.preview(id)?.is_empty());
    }
    let repeated = cat.import(&new, None, |_| Ok(()))?;
    ensure!(repeated.unchanged == 2 && repeated.metadata_updated == 0);
    ensure!(
        db(&root)?.query_row("SELECT COUNT(*) FROM metadata_sources", [], |r| r
            .get::<_, i64>(0))?
            == source_count
    );
    let current: Vec<String> = db(&root)?
        .prepare("SELECT provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(current == provenance);
    cat.undo_relink(&p)?;
    ensure!(paths(&cat)? == before);
    cat.undo_relink(&p)?;
    ensure!(new.join("nested/雪.jpg").exists() && !old.exists());
    Ok(())
}
#[test]
fn interrupted_apply_and_undo_preserve_every_location_and_metadata_choice() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    photo(&old.join("two.jpg"), 80);
    cat.import(&old, None, |_| Ok(()))?;
    let before = paths(&cat)?;
    let id = before[0].0.clone();
    let revision = cat.metadata(&id)?.revision;
    cat.edit_metadata(
        &id,
        revision,
        None,
        &[photocatalog::xmp::Edit::Set {
            namespace: "http://ns.adobe.com/xap/1.0/".into(),
            path: "Rating".into(),
            value: "4".into(),
        }],
    )?;
    let new = temp.path().join("moved");
    fs::rename(&old, &new)?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    for point in [
        RelinkBoundary::BeforeMutation,
        RelinkBoundary::Updated(1),
        RelinkBoundary::BeforeCommit,
    ] {
        ensure!(
            cat.apply_relink_with(&p, |at| {
                if at == point {
                    anyhow::bail!("injected storage failure");
                }
                Ok(())
            })
            .is_err()
        );
        ensure!(paths(&cat)? == before);
        ensure!(cat.relink_plan(&p)?.state == "ready");
    }
    cat.apply_relink(&p)?;
    let applied = paths(&cat)?;
    ensure!(
        cat.undo_relink_with(&p, |at| {
            if matches!(at, RelinkBoundary::Updated(_)) {
                anyhow::bail!("interrupted undo");
            }
            Ok(())
        })
        .is_err()
    );
    ensure!(paths(&cat)? == applied);
    cat.undo_relink(&p)?;
    ensure!(paths(&cat)? == before);
    ensure!(
        cat.metadata(&id)?
            .fields
            .iter()
            .any(|f| f.name == "rating"
                && f.value == Some(photocatalog::xmp::Value::Text("4".into())))
    );
    ensure!(
        db(&root)?.query_row(
            "SELECT COUNT(*) FROM metadata_choices WHERE asset_id=?",
            [id],
            |r| r.get::<_, i64>(0)
        )? > 0
    );
    Ok(())
}
#[test]
fn incorrect_same_name_ambiguous_copies_and_explicit_exceptions() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    photo(&old.join("photo.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let wrong = temp.path().join("wrong");
    photo(&wrong.join("photo.jpg"), 200);
    let p = prepared(&mut cat, prefix(&old, &wrong))?;
    ensure!(cat.relink_items(&p, 0, 1)?[0].status == "mismatch");
    ensure!(cat.apply_relink(&p).is_err());
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    fs::copy(old.join("photo.jpg"), a.join("photo.jpg"))?;
    fs::copy(old.join("photo.jpg"), b.join("photo.jpg"))?;
    let p = prepared(
        &mut cat,
        RelinkScope::Prefix {
            from: PathReference::native(&old),
            destinations: vec![NativePath::from_path(&a), NativePath::from_path(&b)],
        },
    )?;
    ensure!(cat.relink_items(&p, 0, 1)?[0].status == "ambiguous");
    ensure!(cat.apply_relink(&p).is_err());
    let renamed = b.join("renamed.jpg");
    fs::rename(b.join("photo.jpg"), &renamed)?;
    let p = cat.begin_relink(prefix(&old, &wrong))?;
    cat.set_relink_candidates(&p.id, &asset.id, vec![NativePath::from_path(&renamed)])?;
    cat.prepare_relink_batch(&p.id, 10)?;
    cat.apply_relink(&p.id)?;
    ensure!(cat.get(&asset.id)?.original_path == renamed.to_string_lossy());
    Ok(())
}
#[test]
fn replacement_after_preview_and_catalog_change_refuse_stale_plans() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    fs::copy(old.join("one.jpg"), new.join("one.jpg"))?;
    let before = paths(&cat)?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    let target = new.join("one.jpg");
    let replacement = new.join("replacement.jpg");
    fs::copy(&target, &replacement)?;
    fs::rename(&replacement, &target)?;
    ensure!(cat.apply_relink(&p).is_err());
    ensure!(paths(&cat)? == before);
    let p = prepared(&mut cat, prefix(&old, &new))?;
    db(&root)?.execute("UPDATE assets SET fingerprint='changed'", [])?;
    ensure!(cat.apply_relink(&p).is_err());
    ensure!(paths(&cat)? == before);
    Ok(())
}
#[test]
fn legacy_windows_and_unix_exact_component_mapping() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    photo(&old.join("two.jpg"), 80);
    cat.import(&old, None, |_| Ok(()))?;
    let assets = cat.browse(0, 10)?;
    let windows: Vec<u16> = "Z:\\Photos\\2024\\雪.jpg".encode_utf16().collect();
    cat.set_legacy_storage_path(&assets[0].id, PathReference::LegacyWindows(windows))?;
    cat.set_legacy_storage_path(
        &assets[1].id,
        PathReference::LegacyUnix(b"/Volumes/Old/Photos-other/two.jpg".to_vec()),
    )?;
    let new = temp.path().join("restored");
    fs::create_dir_all(new.join("2024"))?;
    fs::copy(&assets[0].original_path, new.join("2024/雪.jpg"))?;
    let p = prepared(
        &mut cat,
        RelinkScope::Prefix {
            from: PathReference::LegacyWindows("Z:/Photos".encode_utf16().collect()),
            destinations: vec![NativePath::from_path(&new)],
        },
    )?;
    ensure!(cat.relink_plan(&p)?.matched == 1);
    cat.apply_relink(&p)?;
    ensure!(cat.get(&assets[1].id)?.original_path == assets[1].original_path);
    let p = prepared(
        &mut cat,
        RelinkScope::Prefix {
            from: PathReference::LegacyUnix(b"/Volumes/Old/Photos".to_vec()),
            destinations: vec![NativePath::from_path(&new)],
        },
    )?;
    ensure!(cat.relink_plan(&p)?.total == 0);
    ensure!(
        cat.begin_relink(RelinkScope::Prefix {
            from: PathReference::LegacyWindows("Z:/Photos/../other".encode_utf16().collect()),
            destinations: vec![NativePath::from_path(&new)]
        })
        .is_err()
    );
    Ok(())
}
#[test]
fn missing_source_requires_explicit_retention_and_collision_never_overwrites() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    photo(&old.join("two.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let assets = cat.browse(0, 10)?;
    let before = paths(&cat)?;
    let p = cat.begin_relink(prefix(&old, &temp.path().join("missing")))?;
    for asset in &assets {
        cat.set_relink_candidates(
            &p.id,
            &asset.id,
            vec![NativePath::from_path(Path::new(&assets[0].original_path))],
        )?;
    }
    cat.prepare_relink_batch(&p.id, 10)?;
    ensure!(cat.relink_plan(&p.id)?.unresolved == 2);
    ensure!(cat.apply_relink(&p.id).is_err());
    ensure!(paths(&cat)? == before);
    let p = prepared(&mut cat, prefix(&old, &temp.path().join("missing")))?;
    for item in cat.relink_items(&p, 0, 10)? {
        cat.exclude_relink_item(&p, item.sequence)?;
    }
    cat.apply_relink(&p)?;
    ensure!(paths(&cat)? == before);
    Ok(())
}
fn observed(path: &Path, mount: &Path, relative: &Path) -> VolumeLocation {
    let volume = MountedVolume {
        mount_path: NativePath::from_path(mount),
        volume_subpath: NativePath::from_path(Path::new("")),
        persistent_identity: Some(
            PersistentVolumeId::new(IdentityScheme::LinuxFilesystemUuid, "fixture-unique-volume")
                .unwrap(),
        ),
        filesystem: "fixture".into(),
        device_number: None,
        issues: vec![],
    };
    VolumeLocation {
        requested_path: NativePath::from_path(path),
        state: LocationState::Available,
        canonical_path: Some(NativePath::from_path(path)),
        volume: Some(volume),
        relative_in_volume: Some(NativePath::from_path(relative)),
        existing_ancestor: None,
        issues: vec![],
    }
}
#[test]
fn offline_browsing_known_volume_reconnect_and_unregistered_distinction() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    let path = old.join("one.jpg");
    photo(&path, 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    cat.bind_storage(&asset.id, &observed(&path, &old, Path::new("one.jpg")))?;
    let before = cat.render_identity(&asset.id)?;
    let incomplete = MountSnapshot {
        mounts: vec![],
        complete: false,
        issues: vec![],
    };
    ensure!(
        cat.reconnect_storage_asset(
            &path,
            &observed(&path, &old, Path::new("one.jpg")),
            "changed content uses normal import",
            &incomplete
        )? == Some(asset.id.clone())
    );

    cat.bind_storage(&asset.id, &observed(&path, &old, Path::new("one.jpg")))?;
    ensure!(cat.render_identity(&asset.id)? == before);
    let new = temp.path().join("new mount");
    fs::rename(&old, &new)?;
    ensure!(
        cat.storage_status(
            &asset.id,
            &MountSnapshot {
                mounts: vec![],
                complete: true,
                issues: vec![]
            }
        )?
        .state
            == "offline"
    );
    ensure!(!cat.preview(&asset.id)?.is_empty());
    let path = new.join("one.jpg");
    let observation = observed(&path, &new, Path::new("one.jpg"));
    let fingerprint = blake3::hash(&fs::read(&path)?).to_hex().to_string();
    let snapshot = MountSnapshot {
        mounts: vec![observation.volume.clone().unwrap()],
        complete: true,
        issues: vec![],
    };
    ensure!(
        cat.reconnect_storage_asset(&path, &observation, "wrong", &snapshot)
            .is_err()
    );
    let duplicate = MountSnapshot {
        mounts: vec![
            observation.volume.clone().unwrap(),
            observation.volume.clone().unwrap(),
        ],
        complete: true,
        issues: vec![],
    };
    ensure!(
        cat.reconnect_storage_asset(&path, &observation, &fingerprint, &duplicate)
            .is_err()
    );
    let incomplete = MountSnapshot {
        mounts: snapshot.mounts.clone(),
        complete: false,
        issues: vec![],
    };
    ensure!(
        cat.reconnect_storage_asset(&path, &observation, &fingerprint, &incomplete)
            .is_err()
    );

    ensure!(
        cat.reconnect_storage_asset(&path, &observation, &fingerprint, &snapshot)?
            == Some(asset.id.clone())
    );
    ensure!(cat.browse(0, 10)?.len() == 1);
    ensure!(cat.get(&asset.id)?.original_path == path.to_string_lossy());
    Ok(())
}
#[cfg(unix)]
#[test]
fn symlinks_and_platform_native_names_remain_safe_and_lossless() -> Result<()> {
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    let (temp, old, _, mut cat) = setup()?;
    #[cfg(target_os = "linux")]
    let name = std::ffi::OsString::from_vec(b"photo-\xff.jpg".to_vec());
    #[cfg(not(target_os = "linux"))]
    let name = std::ffi::OsString::from("photo-雪.jpg");
    photo(&old.join(&name), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    symlink(old.join(&name), new.join(&name))?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    ensure!(cat.relink_items(&p, 0, 1)?[0].status == "unavailable");
    ensure!(cat.apply_relink(&p).is_err());
    fs::remove_file(new.join(&name))?;
    fs::copy(old.join(&name), new.join(&name))?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    cat.apply_relink(&p)?;
    let rows = cat.relink_items(&p, 0, 1)?;
    ensure!(rows[0].destination == Some(NativePath::from_path(&new.join(&name))));
    Ok(())
}

#[test]
fn abrupt_exit_child() -> Result<()> {
    let Ok(root) = std::env::var("PHOTOCATALOG_RELINK_CRASH_ROOT") else {
        return Ok(());
    };
    let plan = std::env::var("PHOTOCATALOG_RELINK_CRASH_PLAN")?;
    let mut cat = Catalog::open(root)?;
    cat.apply_relink_with(&plan, |point| {
        if matches!(point, RelinkBoundary::Updated(_)) {
            std::process::exit(77);
        }
        Ok(())
    })?;
    anyhow::bail!("fault boundary was not reached")
}
#[test]
fn process_exit_mid_transaction_recovers_old_paths_after_restart() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    photo(&old.join("two.jpg"), 80);
    cat.import(&old, None, |_| Ok(()))?;
    let before = paths(&cat)?;
    let new = temp.path().join("new");
    fs::rename(&old, &new)?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    drop(cat);
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "abrupt_exit_child", "--nocapture"])
        .env("PHOTOCATALOG_RELINK_CRASH_ROOT", &root)
        .env("PHOTOCATALOG_RELINK_CRASH_PLAN", &p)
        .output()?;
    ensure!(
        output.status.code() == Some(77),
        "child did not reach fault: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut cat = Catalog::open(&root)?;
    ensure!(paths(&cat)? == before);
    ensure!(cat.relink_plan(&p)?.state == "ready");
    cat.apply_relink(&p)?;
    ensure!(cat.relink_plan(&p)?.state == "applied");
    Ok(())
}
#[test]
fn concurrent_byte_change_after_validation_rolls_back_without_touching_source() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    let target = new.join("one.jpg");
    fs::copy(old.join("one.jpg"), &target)?;
    let original = fs::read(old.join("one.jpg"))?;
    let before = paths(&cat)?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    ensure!(
        cat.apply_relink_with(&p, |point| {
            if point == RelinkBoundary::BeforeMutation {
                fs::write(&target, b"external update must survive")?;
            }
            Ok(())
        })
        .is_err()
    );
    ensure!(paths(&cat)? == before);
    ensure!(fs::read(&target)? == b"external update must survive");
    ensure!(fs::read(old.join("one.jpg"))? == original);
    Ok(())
}

#[test]
fn valid_swaps_keep_asset_ids_and_low_disk_fault_rolls_back() -> Result<()> {
    let (_temp, old, _, mut cat) = setup()?;
    photo(&old.join("a.jpg"), 20);
    photo(&old.join("b.jpg"), 80);
    cat.import(&old, None, |_| Ok(()))?;
    let assets = cat.browse(0, 10)?;
    let before = paths(&cat)?;
    let a = PathBuf::from(&assets[0].original_path);
    let b = PathBuf::from(&assets[1].original_path);
    let tmp = old.join("temporary");
    fs::rename(&a, &tmp)?;
    fs::rename(&b, &a)?;
    fs::rename(tmp, &b)?;
    let p = cat.begin_relink(prefix(&old, &old))?;
    cat.set_relink_candidates(&p.id, &assets[0].id, vec![NativePath::from_path(&b)])?;
    cat.set_relink_candidates(&p.id, &assets[1].id, vec![NativePath::from_path(&a)])?;
    cat.prepare_relink_batch(&p.id, 10)?;
    ensure!(cat.relink_plan(&p.id)?.matched == 2);
    ensure!(
        cat.apply_relink_with(&p.id, |at| {
            if matches!(at, RelinkBoundary::Updated(_)) {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull).into());
            }
            Ok(())
        })
        .is_err()
    );
    ensure!(paths(&cat)? == before);
    cat.apply_relink(&p.id)?;
    ensure!(cat.get(&assets[0].id)?.original_path == b.to_string_lossy());
    ensure!(cat.get(&assets[1].id)?.original_path == a.to_string_lossy());
    cat.undo_relink(&p.id)?;
    ensure!(paths(&cat)? == before);
    Ok(())
}
#[test]
fn missing_sidecar_retains_evidence_until_explicit_source_mapping() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("multi.name.jpg"), 20);
    fs::write(old.join("multi.name.xmp"),b"<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"/></rdf:RDF>")?;
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    let dest = new.join("renamed.multi.jpg");
    fs::copy(old.join("multi.name.jpg"), &dest)?;
    let p = prepared(
        &mut cat,
        RelinkScope::Asset {
            asset_id: asset.id.clone(),
            destinations: vec![NativePath::from_path(&dest)],
        },
    )?;
    let row = cat.relink_items(&p, 0, 1)?.remove(0);
    ensure!(cat.relink_plan(&p)?.unresolved_sources == 1);
    ensure!(cat.apply_relink(&p).is_err());
    let sidecar = cat
        .relink_sources(&p, row.sequence, 0, 10)?
        .into_iter()
        .find(|s| s.status == "missing")
        .unwrap();
    ensure!(sidecar.candidates[0].path == NativePath::from_path(&new.join("renamed.multi.xmp")));

    let embedded = cat
        .relink_sources(&p, row.sequence, 0, 10)?
        .into_iter()
        .find(|s| s.status == "matched")
        .unwrap();
    ensure!(cat.exclude_relink_source(&p, embedded.source_id).is_err());
    cat.exclude_relink_source(&p, sidecar.source_id)?;
    cat.apply_relink(&p)?;
    ensure!(
        db(&root)?.query_row("SELECT COUNT(*) FROM metadata_models", [], |r| r
            .get::<_, i64>(0))?
            == 1
    );
    cat.undo_relink(&p)?;
    // A subsequent explicit source exception can follow a reorganized XMP folder.
    let other = temp.path().join("metadata");
    fs::create_dir(&other)?;
    let xmp = other.join("metadata.xmp");
    fs::copy(old.join("multi.name.xmp"), &xmp)?;
    let p = cat.begin_relink(RelinkScope::Asset {
        asset_id: asset.id,
        destinations: vec![NativePath::from_path(&dest)],
    })?;
    cat.set_relink_source_candidates(&p.id, sidecar.source_id, vec![NativePath::from_path(&xmp)])?;
    cat.prepare_relink_batch(&p.id, 10)?;
    cat.apply_relink(&p.id)?;
    ensure!(cat.relink_plan(&p.id)?.unresolved_sources == 0);
    Ok(())
}
#[test]
fn preparing_pages_resume_without_duplicates_and_new_descendants_stale_the_plan() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    for i in 0..7 {
        photo(&old.join(format!("{i}.jpg")), i * 20);
    }
    cat.import(&old, None, |_| Ok(()))?;
    let new = temp.path().join("new");
    fs::rename(&old, &new)?;
    let p = cat.begin_relink(prefix(&old, &new))?;
    let first = cat.prepare_relink_batch(&p.id, 2)?;
    ensure!(first.total == 2 && first.state == "preparing");
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    while cat.relink_plan(&p.id)?.state == "preparing" {
        cat.prepare_relink_batch(&p.id, 2)?;
    }
    ensure!(cat.relink_plan(&p.id)?.total == 7);
    ensure!(cat.relink_plans("", 1)?[0].id == p.id);
    db(&root)?.execute("INSERT INTO assets(id,location,path_display,state) VALUES('new',X'6e6577','new','pending')",[])?;
    ensure!(cat.apply_relink(&p.id).is_err());
    Ok(())
}

#[test]
fn reverse_undo_lineage_allows_unrelated_work_but_rejects_affected_changes() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let before = cat.render_identity(&asset.id)?;
    let middle = temp.path().join("middle");
    fs::rename(&old, &middle)?;
    let a = prepared(&mut cat, prefix(&old, &middle))?;
    cat.apply_relink(&a)?;
    let new = temp.path().join("new");
    fs::rename(&middle, &new)?;
    let b = prepared(&mut cat, prefix(&middle, &new))?;
    cat.apply_relink(&b)?;
    ensure!(cat.undo_relink(&a).is_err());
    let unrelated = temp.path().join("unrelated");
    photo(&unrelated.join("other.jpg"), 80);
    cat.import(&unrelated, None, |_| Ok(()))?;
    cat.undo_relink(&b)?;
    cat.undo_relink(&a)?;
    ensure!(cat.get(&asset.id)?.original_path == asset.original_path);
    ensure!(cat.render_identity(&asset.id)?.generation == before.generation + 4);
    let p = prepared(&mut cat, prefix(&old, &new))?;
    cat.apply_relink(&p)?;
    db(&root)?.execute(
        "UPDATE assets SET fingerprint='external revision' WHERE id=?",
        [&asset.id],
    )?;
    ensure!(cat.undo_relink(&p).is_err());
    Ok(())
}
#[test]
fn a_new_relink_and_undo_cannot_hide_intervening_metadata_edits() -> Result<()> {
    let (temp, old, _, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let middle = temp.path().join("middle");
    fs::rename(&old, &middle)?;
    let a = prepared(&mut cat, prefix(&old, &middle))?;
    cat.apply_relink(&a)?;
    let revision = cat.metadata(&asset.id)?.revision;
    cat.edit_metadata(
        &asset.id,
        revision,
        None,
        &[photocatalog::xmp::Edit::Set {
            namespace: "http://ns.adobe.com/xap/1.0/".into(),
            path: "Rating".into(),
            value: "5".into(),
        }],
    )?;
    let new = temp.path().join("new");
    fs::rename(&middle, &new)?;
    let b = prepared(&mut cat, prefix(&middle, &new))?;
    cat.apply_relink(&b)?;
    cat.undo_relink(&b)?;
    ensure!(cat.undo_relink(&a).is_err());
    ensure!(cat.get(&asset.id)?.original_path == middle.join("one.jpg").to_string_lossy());
    ensure!(
        cat.metadata(&asset.id)?
            .fields
            .iter()
            .any(|f| f.name == "rating"
                && f.value == Some(photocatalog::xmp::Value::Text("5".into())))
    );
    Ok(())
}

#[test]
fn catalog_owned_cache_files_cannot_become_originals() -> Result<()> {
    let (_temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let target = root.join("previews/identical-original.jpg");
    fs::copy(old.join("one.jpg"), &target)?;
    let p = prepared(
        &mut cat,
        RelinkScope::Asset {
            asset_id: asset.id.clone(),
            destinations: vec![NativePath::from_path(&target)],
        },
    )?;
    ensure!(cat.relink_items(&p, 0, 1)?[0].status == "unavailable");
    ensure!(cat.apply_relink(&p).is_err());
    ensure!(cat.get(&asset.id)?.original_path == asset.original_path);
    Ok(())
}

#[test]
fn same_mount_name_different_volume_content_and_transient_identity_loss_are_safe() -> Result<()> {
    let (_temp, old, root, mut cat) = setup()?;
    let path = old.join("one.jpg");
    photo(&path, 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let observation = observed(&path, &old, Path::new("one.jpg"));
    cat.bind_storage(&asset.id, &observation)?;
    let before: (String, String) = db(&root)?.query_row(
        "SELECT volume_id,relative FROM storage_bindings WHERE asset_id=?",
        [&asset.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut unknown = observation.clone();
    unknown.volume.as_mut().unwrap().persistent_identity = None;
    unknown.relative_in_volume = None;
    cat.bind_storage(&asset.id, &unknown)?;
    let after: (String, String) = db(&root)?.query_row(
        "SELECT volume_id,relative FROM storage_bindings WHERE asset_id=?",
        [&asset.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(before == after);
    let mut different = observation.clone();
    different.volume.as_mut().unwrap().persistent_identity = Some(PersistentVolumeId::new(
        IdentityScheme::LinuxFilesystemUuid,
        "different-volume",
    )?);
    let snapshot = MountSnapshot {
        mounts: vec![different.volume.clone().unwrap()],
        complete: true,
        issues: vec![],
    };
    ensure!(
        cat.reconnect_storage_asset(&path, &different, "different file", &snapshot)
            .is_err()
    );
    ensure!(
        cat.reconnect_storage_asset(&path, &unknown, "different file", &snapshot)
            .is_err()
    );
    let hash = blake3::hash(&fs::read(&path)?).to_hex().to_string();
    ensure!(
        cat.reconnect_storage_asset(&path, &different, &hash, &snapshot)?
            .is_none()
    );
    ensure!(cat.browse(0, 10)?.len() == 1);
    Ok(())
}

#[test]
fn copied_sidecar_with_new_mtime_preserves_explicit_source_selection() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    let original_xmp = old.join("one.xmp");
    let bytes=br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="copy-origin" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"/></rdf:RDF>"#;
    fs::write(&original_xmp, bytes)?;
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let field = view.fields.iter().find(|f| f.name == "rating").unwrap();
    let model = field.candidates[0].model_id;
    cat.resolve_metadata(&asset.id, view.revision, "rating", model)?;
    let prior: Vec<(i64, String)> = db(&root)?
        .prepare("SELECT id,provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    fs::copy(old.join("one.jpg"), new.join("one.jpg"))?;
    let xmp = new.join("one.xmp");
    fs::copy(&original_xmp, &xmp)?;
    let modified = fs::metadata(&original_xmp)?.modified()? + std::time::Duration::from_secs(60);
    fs::OpenOptions::new()
        .write(true)
        .open(&xmp)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;
    ensure!(fs::metadata(&xmp)?.modified()? != fs::metadata(&original_xmp)?.modified()?);
    let p = prepared(&mut cat, prefix(&old, &new))?;
    cat.apply_relink(&p)?;
    let history_before: i64 =
        db(&root)?.query_row("SELECT COUNT(*) FROM metadata_history", [], |r| r.get(0))?;
    let imported = cat.import(&new, None, |_| Ok(()))?;
    ensure!(imported.unchanged == 1 && imported.metadata_updated == 0);
    let current = cat.metadata(&asset.id)?;
    let rating = current.fields.iter().find(|f| f.name == "rating").unwrap();
    ensure!(rating.selected_model == Some(model) && !rating.conflicted);
    let after: Vec<(i64, String)> = db(&root)?
        .prepare("SELECT id,provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(after == prior);
    ensure!(
        db(&root)?.query_row("SELECT COUNT(*) FROM metadata_history", [], |r| r
            .get::<_, i64>(0))?
            == history_before
    );
    let instances = cat.metadata_file_instances(&asset.id, 0, 10)?;
    ensure!(instances.len() == 1);
    ensure!(
        instances[0].provenance["source_location"]["display"].as_str()
            == Some(xmp.to_string_lossy().as_ref())
    );
    ensure!(
        instances[0].provenance["file_revision"]["modified_unix_ns"].as_u64()
            == Some(modified.duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64)
    );
    ensure!(
        cat.metadata_file_instances(&asset.id, instances[0].id, 10)?
            .is_empty()
    );
    cat.import(&new, None, |_| Ok(()))?;
    ensure!(cat.metadata_file_instances(&asset.id, 0, 10)?.len() == 1);
    ensure!(fs::read(&original_xmp)? == bytes);
    Ok(())
}

#[test]
fn foreign_catalog_locators_remain_tagged_through_folder_file_relink_and_undo() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    fs::write(old.join("one.xmp"),br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="foreign" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF>"#)?;
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    #[cfg(unix)]
    let (foreign_root, foreign_photo, foreign_sidecar) = (
        NativePath::WindowsWide("R:\\Photos".encode_utf16().collect()),
        NativePath::WindowsWide("R:\\Photos\\one.jpg".encode_utf16().collect()),
        NativePath::WindowsWide("R:\\Photos\\one.xmp".encode_utf16().collect()),
    );
    #[cfg(windows)]
    let (foreign_root, foreign_photo, foreign_sidecar) = (
        NativePath::UnixBytes(b"/Volumes/Old/Photos".to_vec()),
        NativePath::UnixBytes(b"/Volumes/Old/Photos/one.jpg".to_vec()),
        NativePath::UnixBytes(b"/Volumes/Old/Photos/one.xmp".to_vec()),
    );
    let encoded = |path: &NativePath| match path {
        NativePath::UnixBytes(v) => v.clone(),
        NativePath::WindowsWide(v) => v.iter().flat_map(|u| u.to_le_bytes()).collect(),
    };
    // Simulate a closed catalog last used on the other OS. Raw current locators
    // and their explicit tags move together; immutable observations stay intact.
    let connection = db(&root)?;
    connection.execute(
        "UPDATE assets SET location=?2,path_display='foreign original' WHERE id=?1",
        rusqlite::params![asset.id, encoded(&foreign_photo)],
    )?;
    connection.execute("UPDATE storage_bindings SET reference=?2,native_path=?3,volume_id=NULL,relative=NULL,file_key=NULL WHERE asset_id=?1",rusqlite::params![asset.id,serde_json::to_string(&PathReference::Native(foreign_photo.clone()))?,serde_json::to_string(&foreign_photo)?])?;
    connection.execute(
        "UPDATE metadata_sources SET locator=?2 WHERE asset_id=?1 AND kind='embedded'",
        rusqlite::params![asset.id, encoded(&foreign_photo)],
    )?;
    connection.execute(
        "UPDATE metadata_sources SET locator=?2 WHERE asset_id=?1 AND kind='sidecar'",
        rusqlite::params![asset.id, encoded(&foreign_sidecar)],
    )?;
    drop(connection);
    drop(cat);
    let mut cat = Catalog::open(&root)?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    fs::copy(old.join("one.jpg"), new.join("one.jpg"))?;
    fs::copy(old.join("one.xmp"), new.join("one.xmp"))?;
    let p = prepared(
        &mut cat,
        RelinkScope::Prefix {
            from: PathReference::Native(foreign_root),
            destinations: vec![NativePath::from_path(&new)],
        },
    )?;
    let item = cat.relink_items(&p, 0, 1)?.remove(0);
    ensure!(item.status == "matched");
    let sources = cat.relink_sources(&p, item.sequence, 0, 10)?;
    ensure!(sources.iter().any(
        |s| s.original == PathReference::Native(foreign_sidecar.clone()) && s.status == "matched"
    ));
    cat.apply_relink(&p)?;
    cat.undo_relink(&p)?;
    ensure!(
        db(&root)?.query_row("SELECT location FROM assets WHERE id=?", [&asset.id], |r| r
            .get::<_, Vec<u8>>(0))?
            == encoded(&foreign_photo)
    );
    let renamed = new.join("renamed.jpg");
    fs::rename(new.join("one.jpg"), &renamed)?;
    fs::rename(new.join("one.xmp"), new.join("renamed.xmp"))?;
    let p = prepared(
        &mut cat,
        RelinkScope::Asset {
            asset_id: asset.id.clone(),
            destinations: vec![NativePath::from_path(&renamed)],
        },
    )?;
    ensure!(cat.relink_plan(&p)?.unresolved_sources == 0);
    cat.apply_relink(&p)?;
    ensure!(cat.get(&asset.id)?.original_path == renamed.to_string_lossy());
    Ok(())
}
#[test]
fn untagged_legacy_locators_require_explicit_bounded_encoding_declaration() -> Result<()> {
    let (_temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    db(&root)?.execute("DELETE FROM storage_bindings WHERE asset_id=?", [&asset.id])?;
    let status = cat.storage_status(
        &asset.id,
        &MountSnapshot {
            mounts: vec![],
            complete: true,
            issues: vec![],
        },
    )?;
    ensure!(matches!(status.current, PathReference::Unspecified(_)));
    #[cfg(unix)]
    let encoding = photocatalog::catalog_storage::StorageEncoding::Unix;
    #[cfg(windows)]
    let encoding = photocatalog::catalog_storage::StorageEncoding::Windows;
    let progress = cat.declare_storage_encoding(encoding, 0, 1)?;
    ensure!(progress.declared == 1);
    ensure!(
        cat.declare_storage_encoding(encoding, progress.scanned_through, 1)?
            .declared
            == 0
    );
    cat.record_storage_path(&asset.id, &NativePath::from_path(&old.join("one.jpg")))?;
    Ok(())
}

#[test]
fn same_path_timestamp_and_same_byte_replacement_preserve_choice_and_record_instances() -> Result<()>
{
    let (_temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    let xmp = old.join("one.xmp");
    let bytes = br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="same-path" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"/></rdf:RDF>"#;
    fs::write(&xmp, bytes)?;
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let view = cat.metadata(&asset.id)?;
    let model = view
        .fields
        .iter()
        .find(|f| f.name == "rating")
        .unwrap()
        .candidates[0]
        .model_id;
    cat.resolve_metadata(&asset.id, view.revision, "rating", model)?;
    let revision = cat.metadata(&asset.id)?.revision;
    let previous: Vec<(i64, String)> = db(&root)?
        .prepare("SELECT id,provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let original_time = fs::metadata(&xmp)?.modified()?;
    for step in 1..=2 {
        if step == 2 {
            let replacement = old.join("replacement.tmp");
            fs::write(&replacement, bytes)?;
            fs::remove_file(&xmp)?;
            fs::rename(replacement, &xmp)?;
        }
        let modified = original_time + std::time::Duration::from_secs(60 * step);
        fs::OpenOptions::new()
            .write(true)
            .open(&xmp)?
            .set_times(fs::FileTimes::new().set_modified(modified))?;
        let report = cat.import(&old, None, |_| Ok(()))?;
        ensure!(report.unchanged == 1 && report.metadata_updated == 0);
        let view = cat.metadata(&asset.id)?;
        let rating = view.fields.iter().find(|f| f.name == "rating").unwrap();
        ensure!(
            view.revision == revision && rating.selected_model == Some(model) && !rating.conflicted
        );
        let instances = cat.metadata_file_instances(&asset.id, 0, 10)?;
        ensure!(instances.len() == step as usize);
        let latest = &instances.last().unwrap().provenance;
        ensure!(
            latest["source_location"]["display"].as_str() == Some(xmp.to_string_lossy().as_ref())
        );
        ensure!(
            latest["file_revision"]["modified_unix_ns"].as_u64()
                == Some(modified.duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64)
        );
    }
    let after: Vec<(i64, String)> = db(&root)?
        .prepare("SELECT id,provenance FROM metadata_observations ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(after == previous);
    // Actual bytes still create a new observation, even when only one value changes.
    fs::write(
        &xmp,
        String::from_utf8(bytes.to_vec())?.replace("Rating=\"4\"", "Rating=\"3\""),
    )?;
    ensure!(cat.import(&old, None, |_| Ok(()))?.metadata_updated == 1);
    ensure!(cat.metadata(&asset.id)?.revision > revision);
    Ok(())
}

#[test]
fn prefix_skips_unknown_rows_and_explicit_candidate_does_not_evaluate_fallback() -> Result<()> {
    let (temp, old, root, mut cat) = setup()?;
    photo(&old.join("one.jpg"), 20);
    photo(&old.join("two.jpg"), 30);
    cat.import(&old, None, |_| Ok(()))?;
    let assets = cat.browse(0, 10)?;
    let unknown = assets
        .iter()
        .find(|a| a.original_path.ends_with("two.jpg"))
        .unwrap();
    db(&root)?.execute(
        "DELETE FROM storage_bindings WHERE asset_id=?",
        [&unknown.id],
    )?;
    let new = temp.path().join("new");
    fs::create_dir(&new)?;
    fs::copy(old.join("one.jpg"), new.join("one.jpg"))?;
    fs::copy(old.join("two.jpg"), new.join("two.jpg"))?;
    let p = prepared(&mut cat, prefix(&old, &new))?;
    ensure!(cat.relink_items(&p, 0, 10)?.len() == 1);
    let p = cat.begin_relink(prefix(&old, &new))?;
    cat.set_relink_candidates(
        &p.id,
        &unknown.id,
        vec![NativePath::from_path(&new.join("two.jpg"))],
    )?;
    while cat.relink_plan(&p.id)?.state == "preparing" {
        cat.prepare_relink_batch(&p.id, 1)?;
    }
    ensure!(cat.relink_items(&p.id, 0, 10)?.len() == 2);
    Ok(())
}

#[test]
fn excluding_prepared_sidecar_swap_rechecks_retained_locator_collision() -> Result<()> {
    let (_temp, old, root, mut cat) = setup()?;
    let original = old.join("one.jpg");
    photo(&original, 20);
    let a = old.join("one.xmp");
    let b = old.join("one.jpg.xmp");
    let xml = br#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="same" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3"/></rdf:RDF>"#;
    fs::write(&a, xml)?;
    fs::write(&b, xml)?;
    cat.import(&old, None, |_| Ok(()))?;
    let asset = cat.browse(0, 1)?.remove(0);
    let sources: Vec<(i64,String)> = db(&root)?.prepare("SELECT id,display FROM metadata_sources WHERE asset_id=? AND kind='sidecar' ORDER BY id")?.query_map([&asset.id], |r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    ensure!(sources.len() == 2);
    let p = cat.begin_relink(RelinkScope::Asset {
        asset_id: asset.id.clone(),
        destinations: vec![NativePath::from_path(&original)],
    })?;
    for (id, display) in &sources {
        let other = if display == a.to_string_lossy().as_ref() {
            &b
        } else {
            &a
        };
        cat.set_relink_source_candidates(&p.id, *id, vec![NativePath::from_path(other)])?;
    }
    cat.prepare_relink_batch(&p.id, 1)?;
    ensure!(cat.relink_plan(&p.id)?.unresolved_sources == 0);
    cat.exclude_relink_source(&p.id, sources[0].0)?;
    let err = cat.apply_relink(&p.id).unwrap_err();
    ensure!(
        err.to_string().contains("metadata source collision"),
        "{err:#}"
    );
    let after: Vec<(i64,String)> = db(&root)?.prepare("SELECT id,display FROM metadata_sources WHERE asset_id=? AND kind='sidecar' ORDER BY id")?.query_map([&asset.id], |r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    ensure!(sources == after);
    ensure!(fs::read(a)? == xml && fs::read(b)? == xml);
    Ok(())
}
