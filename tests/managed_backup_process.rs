#![cfg(unix)]

use anyhow::{Result, ensure};
use photocatalog::{
    CURRENT_SCHEMA_VERSION, Catalog,
    application::U64,
    catalog_backup::{
        CancellationToken, Limits, Phase,
        managed::{self, ProcessEvent, ProcessTestFault, Receipt, Request},
    },
    catalog_session::PhysicalObjectId,
    storage_volume::NativePath,
};
use rusqlite::Connection;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

fn executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_photocatalog"))
}
fn limits() -> Limits {
    Limits {
        min_free_bytes: 0,
        pages_per_step: 1,
        max_seconds: 30,
        ..Limits::default()
    }
}
fn actual(db: &Connection) -> Result<PhysicalObjectId> {
    const FILE_IDENTITY_V1: i32 = 0x5043_4301;
    let mut identity = [0u64; 2];
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            db.handle(),
            c"main".as_ptr(),
            FILE_IDENTITY_V1,
            identity.as_mut_ptr().cast(),
        )
    };
    ensure!(
        code == rusqlite::ffi::SQLITE_OK,
        "actual SQLite identity unavailable"
    );
    Ok(PhysicalObjectId::Unix {
        device: U64(identity[0]),
        inode: U64(identity[1]),
    })
}

fn assert_reaped(pid: u32) {
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

fn process_probe() -> (
    Arc<Mutex<Vec<ProcessEvent>>>,
    impl FnMut(ProcessEvent) + Send + 'static,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    (events, move |event| observed.lock().unwrap().push(event))
}

fn assert_siblings_reaped(events: &[ProcessEvent]) -> (u32, u32) {
    let (backup, filesystem) = events
        .iter()
        .find_map(|event| match event {
            ProcessEvent::Spawned { backup, filesystem } => Some((*backup, *filesystem)),
            _ => None,
        })
        .expect("spawned process identities");
    assert!(
        events
            .iter()
            .any(|event| *event == ProcessEvent::BackupReaped { backup }),
        "backup checked reap"
    );
    assert!(
        events
            .iter()
            .any(|event| *event == ProcessEvent::FilesystemReaped { filesystem }),
        "filesystem checked reap"
    );
    assert_reaped(backup);
    assert_reaped(filesystem);
    (backup, filesystem)
}

#[test]
fn managed_create_inspect_restore_are_process_owned_and_preserve_online_snapshot() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    let _catalog = Catalog::open(&source)?;
    let writer = Connection::open(source.join("catalog.sqlite3"))?;
    writer.execute_batch("CREATE TABLE managed_backup_probe(id INTEGER PRIMARY KEY, value INTEGER NOT NULL); INSERT INTO managed_backup_probe VALUES(1, 7)")?;
    let expected = actual(&writer)?;
    let bundle = temp.path().join("bundle");
    let cancel = CancellationToken::default();
    let mut updated = false;
    let Receipt::Backup(created) = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&bundle),
            expected_source: expected,
        },
        limits(),
        &cancel,
        |progress| {
            if progress.phase == Phase::Snapshot && !updated {
                writer.execute("UPDATE managed_backup_probe SET value=8 WHERE id=1", [])?;
                updated = true;
            }
            Ok(())
        },
    )?
    else {
        anyhow::bail!("create returned restore receipt")
    };
    assert!(updated);
    let backup = Connection::open(bundle.join("catalog.sqlite3"))?;
    assert_eq!(
        backup.query_row("SELECT value FROM managed_backup_probe", [], |row| row
            .get::<_, i64>(0))?,
        7
    );
    drop(backup);

    let Receipt::Backup(inspected) = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Inspect {
            bundle: NativePath::from_path(&bundle),
        },
        limits(),
        &CancellationToken::default(),
        |_| Ok(()),
    )?
    else {
        anyhow::bail!("inspect returned restore receipt")
    };
    assert_eq!(inspected, created);

    let destination = temp.path().join("restored");
    let Receipt::Restore(restored) = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Restore {
            bundle: NativePath::from_path(&bundle),
            destination: NativePath::from_path(&destination),
        },
        limits(),
        &CancellationToken::default(),
        |_| Ok(()),
    )?
    else {
        anyhow::bail!("restore returned backup receipt")
    };
    assert_eq!(restored.backup, created);
    assert_eq!(restored.schema_version, CURRENT_SCHEMA_VERSION);
    assert!(
        photocatalog::catalog_backup::restore_status(&destination)?
            .unwrap()
            .jobs_held
    );
    let restored_db = Connection::open(destination.join("catalog.sqlite3"))?;
    assert_eq!(
        restored_db.query_row("SELECT value FROM managed_backup_probe", [], |row| row
            .get::<_, i64>(0))?,
        7
    );
    Ok(())
}

#[test]
fn managed_cancel_and_post_open_swap_retain_incomplete_destinations() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    let writer = Connection::open(source.join("catalog.sqlite3"))?;
    writer.execute("CREATE TABLE large_probe(bytes BLOB NOT NULL)", [])?;
    writer.execute(
        "INSERT INTO large_probe VALUES(?1)",
        [vec![9u8; 4 * 1024 * 1024]],
    )?;
    let expected = actual(&writer)?;
    drop(writer);

    let canceled = temp.path().join("canceled");
    let cancel = CancellationToken::default();
    let callback_cancel = cancel.clone();
    let result = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&canceled),
            expected_source: expected,
        },
        limits(),
        &cancel,
        move |progress| {
            if progress.phase == Phase::Copy {
                callback_cancel.cancel();
            }
            Ok(())
        },
    );
    assert!(result.unwrap_err().to_string().contains("cancel"));
    assert!(canceled.join(".photocatalog-pending.json").is_file());
    assert!(std::fs::metadata(canceled.join("catalog.sqlite3"))?.len() > 0);
    assert!(!canceled.join("photocatalog-backup.json").exists());

    let replacement_root = temp.path().join("replacement");
    drop(Catalog::open(&replacement_root)?);
    let replaced = temp.path().join("replaced");
    let mut swapped = false;
    let result = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&replaced),
            expected_source: expected,
        },
        limits(),
        &CancellationToken::default(),
        |progress| {
            if progress.phase == Phase::Snapshot && !swapped {
                std::fs::rename(
                    source.join("catalog.sqlite3"),
                    source.join("original.sqlite3"),
                )?;
                std::fs::rename(
                    replacement_root.join("catalog.sqlite3"),
                    source.join("catalog.sqlite3"),
                )?;
                swapped = true;
            }
            Ok(())
        },
    );
    assert!(swapped);
    assert!(format!("{:#}", result.unwrap_err()).contains("changed while held"));
    assert!(replaced.join(".photocatalog-pending.json").is_file());
    Ok(())
}

#[test]
fn managed_restore_refuses_overwrite_and_preserves_existing_destination() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    let db = Connection::open(source.join("catalog.sqlite3"))?;
    let expected = actual(&db)?;
    drop(db);
    let bundle = temp.path().join("bundle");
    managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&bundle),
            expected_source: expected,
        },
        limits(),
        &CancellationToken::default(),
        |_| Ok(()),
    )?;
    let destination = temp.path().join("existing");
    std::fs::create_dir(&destination)?;
    std::fs::write(destination.join("keep"), b"unchanged")?;
    let error = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Restore {
            bundle: NativePath::from_path(&bundle),
            destination: NativePath::from_path(&destination),
        },
        limits(),
        &CancellationToken::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("new directory"));
    assert_eq!(std::fs::read(destination.join("keep"))?, b"unchanged");
    Ok(())
}

#[test]
fn parent_progress_failure_reaps_sql_then_active_filesystem_sibling() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    let db = Connection::open(source.join("catalog.sqlite3"))?;
    let expected = actual(&db)?;
    drop(db);
    let bundle = temp.path().join("progress-failed");
    let (events, probe) = process_probe();
    let error = managed::run_process_with_probe(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&bundle),
            expected_source: expected,
        },
        limits(),
        &CancellationToken::default(),
        |progress| {
            if progress.phase == Phase::Snapshot {
                anyhow::bail!("injected parent progress failure")
            }
            Ok(())
        },
        ProcessTestFault::None,
        probe,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("injected parent progress failure"));
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProcessEvent::FilesystemActive { .. }))
    );
    let (backup, filesystem) = assert_siblings_reaped(&events);
    let b = events
        .iter()
        .position(|event| *event == ProcessEvent::BackupReaped { backup })
        .unwrap();
    let f = events
        .iter()
        .position(|event| *event == ProcessEvent::FilesystemReaped { filesystem })
        .unwrap();
    assert!(b < f, "forced retirement must end SQL before raw custody");
    assert!(bundle.join(".photocatalog-pending.json").is_file());
    assert!(!bundle.join("photocatalog-backup.json").exists());
    Ok(())
}

#[test]
fn protocol_loss_and_wait_retry_keep_both_exact_owners_until_reap() -> Result<()> {
    for fault in [
        ProcessTestFault::ParentProtocolAfterFilesystemRequest,
        ProcessTestFault::LoseTerminal,
        ProcessTestFault::FailBackupWaitOnce,
    ] {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        drop(Catalog::open(&source)?);
        let db = Connection::open(source.join("catalog.sqlite3"))?;
        let expected = actual(&db)?;
        drop(db);
        let bundle = temp.path().join("faulted");
        let (events, probe) = process_probe();
        let error = managed::run_process_with_probe(
            executable(),
            uuid::Uuid::new_v4().to_string(),
            Request::Create {
                source: NativePath::from_path(&source),
                bundle: NativePath::from_path(&bundle),
                expected_source: expected,
            },
            limits(),
            &CancellationToken::default(),
            |_| Ok(()),
            fault,
            probe,
        )
        .unwrap_err();
        let detail = format!("{error:#}");
        match fault {
            ProcessTestFault::ParentProtocolAfterFilesystemRequest => {
                assert!(detail.contains("injected malformed managed backup reply"));
            }
            ProcessTestFault::LoseTerminal => {
                assert!(detail.contains("injected lost managed backup terminal reply"));
            }
            ProcessTestFault::FailBackupWaitOnce => {
                assert!(detail.contains("retained exact child for retry"));
            }
            ProcessTestFault::None => unreachable!(),
        }
        let events = events.lock().unwrap();
        let (backup, filesystem) = assert_siblings_reaped(&events);
        let b = events
            .iter()
            .position(|event| *event == ProcessEvent::BackupReaped { backup })
            .unwrap();
        let f = events
            .iter()
            .position(|event| *event == ProcessEvent::FilesystemReaped { filesystem })
            .unwrap();
        if fault == ProcessTestFault::ParentProtocolAfterFilesystemRequest {
            assert!(b < f, "forced retirement must end SQL before raw custody");
        } else {
            assert!(f < b, "normal F drain must precede B terminal reap");
        }
        if fault == ProcessTestFault::FailBackupWaitOnce {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ProcessEvent::BackupWaitRetry { .. }))
            );
        }
    }

    // A failed generation cannot clear early: this fresh operation is admitted
    // only after the preceding calls returned with both ESRCH proofs.
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    drop(Catalog::open(&source)?);
    let db = Connection::open(source.join("catalog.sqlite3"))?;
    let expected = actual(&db)?;
    drop(db);
    let Receipt::Backup(_) = managed::run_process(
        executable(),
        uuid::Uuid::new_v4().to_string(),
        Request::Create {
            source: NativePath::from_path(&source),
            bundle: NativePath::from_path(&temp.path().join("fresh")),
            expected_source: expected,
        },
        limits(),
        &CancellationToken::default(),
        |_| Ok(()),
    )?
    else {
        anyhow::bail!("fresh create returned restore receipt")
    };
    Ok(())
}
