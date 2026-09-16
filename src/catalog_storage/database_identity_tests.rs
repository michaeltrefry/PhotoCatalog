use super::*;
use std::{os::fd::AsRawFd, process::Command};

#[test]
fn opened_identity_distinguishes_retained_pin_during_path_aba() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("catalog.sqlite3");
    let saved = temp.path().join("original.sqlite3");
    let replacement = temp.path().join("replacement.sqlite3");
    drop(Connection::open(&path)?);
    let held = File::open(&path)?;
    let expected = object_key(&held)?;
    std::fs::rename(&path, &saved)?;
    // SQLite now owns B while our retained authority still names A.
    let db = Connection::open(&path)?;
    assert_ne!(sqlite_opened_object(&db)?, expected);
    // HAS_MOVED alone passes while the current pathname still names B.
    let mut moved = -1i32;
    assert_eq!(
        unsafe {
            rusqlite::ffi::sqlite3_file_control(
                db.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut i32).cast(),
            )
        },
        rusqlite::ffi::SQLITE_OK
    );
    assert_eq!(moved, 0);
    assert!(
        verify_database_object(&db, &held)
            .unwrap_err()
            .to_string()
            .contains("retained pin")
    );
    // Restore A: pathname observations match the original review again, but
    // the opened descriptor is still B. No source query is needed to detect it.
    std::fs::rename(&path, &replacement)?;
    std::fs::rename(&saved, &path)?;
    assert_eq!(object_key(&File::open(&path)?)?, expected);
    assert_ne!(sqlite_opened_object(&db)?, expected);
    assert!(verify_database_object(&db, &held).is_err());
    Ok(())
}

#[test]
fn matching_opened_identity_still_rejects_moved_path_and_unsupported_storage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("catalog.sqlite3");
    let db = Connection::open(&path)?;
    let held = File::open(&path)?;
    verify_database_object(&db, &held)?;
    std::fs::rename(&path, temp.path().join("moved.sqlite3"))?;
    assert_eq!(sqlite_opened_object(&db)?, object_key(&held)?);
    assert!(verify_database_object(&db, &held).is_err());
    assert!(sqlite_opened_object(&Connection::open_in_memory()?).is_err());
    Ok(())
}

fn lock(file: &File, kind: libc::c_short) -> std::io::Result<()> {
    let mut region: libc::flock = unsafe { std::mem::zeroed() };
    region.l_type = kind;
    region.l_whence = libc::SEEK_SET as libc::c_short;
    region.l_start = 0;
    region.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &region) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// A fresh process is necessary: POSIX record locks are process scoped. This
// helper is inert during an ordinary test run and never opens user data.
#[test]
fn posix_lock_contender() -> Result<()> {
    let Some(path) = std::env::var_os("PHOTOCATALOG_TEST_IDENTITY_LOCK_PATH") else {
        return Ok(());
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let should_block = std::env::var("PHOTOCATALOG_TEST_IDENTITY_LOCK_BLOCKED")? == "1";
    let result = lock(&file, libc::F_WRLCK as libc::c_short);
    if should_block {
        let error = result.expect_err("identity observation released the parent's POSIX lock");
        assert!(matches!(
            error.raw_os_error(),
            Some(libc::EACCES | libc::EAGAIN)
        ));
    } else {
        result?;
    }
    Ok(())
}

#[test]
fn opened_identity_observation_preserves_live_posix_lock() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("catalog.sqlite3");
    let db = Connection::open(&path)?;
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    lock(&held, libc::F_WRLCK as libc::c_short)?;
    let contender = |blocked: bool| -> Result<()> {
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "catalog_storage::database_identity_tests::posix_lock_contender",
                "--nocapture",
            ])
            .env("PHOTOCATALOG_TEST_IDENTITY_LOCK_PATH", &path)
            .env(
                "PHOTOCATALOG_TEST_IDENTITY_LOCK_BLOCKED",
                if blocked { "1" } else { "0" },
            )
            .output()?;
        ensure!(
            output.status.success(),
            "lock contender failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        Ok(())
    };
    contender(true)?;
    for _ in 0..32 {
        assert_eq!(sqlite_opened_object(&db)?, object_key(&held)?);
        verify_database_object(&db, &held)?;
    }
    contender(true)?;
    lock(&held, libc::F_UNLCK as libc::c_short)?;
    contender(false)?;
    Ok(())
}
