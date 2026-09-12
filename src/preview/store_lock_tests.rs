//! Deterministic shared-description lifetime coverage; no timing sleeps/forks.
use super::*;

fn config(root: &Path) -> StoreConfig {
    StoreConfig {
        manifest_root: root.join("manifest"),
        thumbnail_root: root.join("thumb"),
        large_root: root.join("large"),
        layout: Layout::Flat,
        thumbnail_bytes: 1024,
        large_bytes: 1024,
    }
}

#[cfg(unix)]
#[test]
fn store_drop_releases_retained_manifest_and_tier_descriptions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = config(root.path());
    let store = PreviewStore::open(config.clone(), &[])?;
    let retained = [
        store._lock.0.try_clone()?,
        store._tier_locks[0].0.try_clone()?,
        store._tier_locks[1].0.try_clone()?,
    ];
    assert!(PreviewStore::open(config.clone(), &[]).is_err());
    assert!(PreviewStore::open(config.clone(), &[]).is_err());
    drop(store);
    let next = PreviewStore::open(config.clone(), &[])?;
    drop(retained);
    assert!(PreviewStore::open(config, &[]).is_err());
    drop(next);
    Ok(())
}

fn contender(path: &Path) -> Result<File> {
    Ok(File::options().read(true).write(true).open(path)?)
}

#[cfg(unix)]
#[test]
fn acquired_guard_releases_on_error_and_unwind_without_unlocking_new_owner() -> Result<()> {
    for unwind in [false, true] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("lock");
        File::create(&path)?;
        let mut retained = None;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let file = contender(&path)?;
            file.try_lock_exclusive()?;
            let guard = AcquiredPreviewLock(file);
            retained = Some(guard.0.try_clone()?);
            assert!(contender(&path)?.try_lock_exclusive().is_err());
            if unwind {
                panic!("controlled failure after preview lock acquisition");
            }
            anyhow::bail!("controlled error after preview lock acquisition")
        }));
        if unwind {
            assert!(outcome.is_err());
        } else {
            assert!(outcome.unwrap().is_err());
        }
        let next = contender(&path)?;
        next.try_lock_exclusive()?;
        let next = AcquiredPreviewLock(next);
        drop(retained);
        assert!(contender(&path)?.try_lock_exclusive().is_err());
        drop(next);
        contender(&path)?.try_lock_exclusive()?;
    }
    Ok(())
}

#[test]
fn constructor_failure_releases_manifest_and_already_acquired_tier() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = config(root.path());
    fs::create_dir_all(&config.large_root)?;
    let invalid = config.large_root.join(".photocatalog-preview-owner");
    fs::write(&invalid, b"wrong owner")?;
    for _ in 0..2 {
        let error = PreviewStore::open(config.clone(), &[]).err().unwrap();
        assert!(error.to_string().contains("belongs to another manifest"));
        assert_eq!(fs::read(&invalid)?, b"wrong owner");
        // All three lock acquisitions preceded the identity validation error.
        for path in [
            config.manifest_root.join("preview.lock"),
            config.thumbnail_root.join(".photocatalog-preview-owner"),
            invalid.clone(),
        ] {
            let file = contender(&path)?;
            file.try_lock_exclusive()?;
            drop(AcquiredPreviewLock(file));
        }
    }
    fs::write(&invalid, [])?;
    let store = PreviewStore::open(config.clone(), &[])?;
    assert!(PreviewStore::open(config, &[]).is_err());
    drop(store);
    Ok(())
}

#[cfg(unix)]
#[test]
fn relocation_lock_moves_through_cleanup_and_releases_retained_descriptions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = config(root.path());
    let mut store = PreviewStore::open(config.clone(), &[])?;
    let identity = store.identity.clone();
    let target = root.path().join("relocated");
    let old_duplicate = store._tier_locks[0].0.try_clone()?;
    store.begin_relocation(Tier::Thumbnail, &target, &[])?;
    let target_duplicate = store._relocation_lock.as_ref().unwrap().0.try_clone()?;
    assert!(relocation::lock_root(&target, &identity, Tier::Thumbnail, Layout::Flat).is_err());
    drop(store);
    // A copy-phase restart reacquires the target as the extra location.
    let mut store = PreviewStore::open(config.clone(), &[])?;
    assert!(relocation::lock_root(&target, &identity, Tier::Thumbnail, Layout::Flat).is_err());
    let progress = store.relocation_step(Tier::Thumbnail, 10, 1024)?;
    assert_eq!(progress.phase, "cleanup");
    // Both old and new locations remain owned until cleanup commits.
    assert!(
        relocation::lock_root(
            &config.thumbnail_root,
            &identity,
            Tier::Thumbnail,
            Layout::Flat
        )
        .is_err()
    );
    assert!(relocation::lock_root(&target, &identity, Tier::Thumbnail, Layout::Flat).is_err());
    let cleanup_config = store.configuration().clone();
    drop(store);
    // A cleanup-phase restart reacquires the old source as the extra location.
    let mut store = PreviewStore::open(cleanup_config, &[])?;
    assert!(
        relocation::lock_root(
            &config.thumbnail_root,
            &identity,
            Tier::Thumbnail,
            Layout::Flat
        )
        .is_err()
    );
    assert!(store.relocation_step(Tier::Thumbnail, 10, 1024)?.complete);
    let old_next = relocation::lock_root(
        &config.thumbnail_root,
        &identity,
        Tier::Thumbnail,
        Layout::Flat,
    )?;
    drop(old_duplicate);
    assert!(
        relocation::lock_root(
            &config.thumbnail_root,
            &identity,
            Tier::Thumbnail,
            Layout::Flat
        )
        .is_err()
    );
    assert!(relocation::lock_root(&target, &identity, Tier::Thumbnail, Layout::Flat).is_err());
    let new_config = store.configuration().clone();
    drop(store);
    let next = PreviewStore::open(new_config.clone(), &[])?;
    drop(target_duplicate);
    assert!(PreviewStore::open(new_config, &[]).is_err());
    drop(next);
    drop(old_next);
    Ok(())
}

#[test]
fn contended_second_tier_does_not_unlock_owner_or_leak_earlier_acquisitions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let config = config(root.path());
    let store = PreviewStore::open(config.clone(), &[])?;
    let identity = store.identity.clone();
    drop(store);
    let owner = relocation::lock_root(&config.large_root, &identity, Tier::Large, Layout::Flat)?;
    for _ in 0..2 {
        let error = PreviewStore::open(config.clone(), &[]).err().unwrap();
        assert!(error.to_string().contains("preview location is owned"));
        assert!(
            relocation::lock_root(&config.large_root, &identity, Tier::Large, Layout::Flat)
                .is_err()
        );
        let first = relocation::lock_root(
            &config.thumbnail_root,
            &identity,
            Tier::Thumbnail,
            Layout::Flat,
        )?;
        drop(first);
        let manifest = contender(&config.manifest_root.join("preview.lock"))?;
        manifest.try_lock_exclusive()?;
        drop(AcquiredPreviewLock(manifest));
    }
    drop(owner);
    PreviewStore::open(config, &[])?;
    Ok(())
}
