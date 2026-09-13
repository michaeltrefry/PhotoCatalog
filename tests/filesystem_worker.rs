//! Actual configured F child; no SQLite connection or native descendant is
//! created in these tests. SQL overlap is qualified with the managed SQL owner.
use anyhow::Result;
use photocatalog::{
    application::U64,
    catalog_session::{BootstrapMode, CatalogFilesystem, LeaseId, PrepareCatalog},
    filesystem_worker::{
        client::Client,
        wire::{AdmissionState, Phase},
    },
    storage_volume::NativePath,
};
use std::{fs, sync::atomic::AtomicBool};

struct NoDependents(Client);
impl Drop for NoDependents {
    fn drop(&mut self) {
        // This fixture never opens SQL or launches a native descendant. Preserve
        // that precondition when changing this test's explicit failure cleanup.
        if self.0.status().phase != Phase::Stopped {
            let _ = self.0.terminate_after_dependents_drained();
        }
    }
}

#[test]
fn configured_child_retains_admission_and_retries_checked_stop_after_exact_cleanup() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = fs::canonicalize(temp.path())?;
    let worker = NoDependents(Client::spawn(
        assert_cmd::cargo::cargo_bin!("photocatalog"),
        vec![],
    )?);
    let pid = worker.0.pid();
    eprintln!("filesystem fixture owned pid={pid}");
    let mut request = PrepareCatalog {
        operation: U64(1),
        session: LeaseId::new(),
        mode: BootstrapMode::DesktopCreate,
        root: NativePath::from_path(&root.join("catalog")),
        manifest_root: NativePath::from_path(&root.join("manifest")),
        import_source: None,
    };
    let bootstrap = worker
        .0
        .prepare_catalog(&request, &AtomicBool::new(false))?;
    assert!(bootstrap.catalog.created && bootstrap.manifest.created);
    let snapshot = worker
        .0
        .admission_status(request.operation, &request.session)?
        .unwrap();
    assert_eq!(snapshot.state, AdmissionState::Prepared);
    assert_eq!(snapshot.bootstrap.as_ref(), Some(&bootstrap));
    // Stop refuses the held admission; it must leave an exact cleanup route.
    assert!(worker.0.try_shutdown().is_err());
    worker
        .0
        .abandon_prepare(request.operation, &request.session)?;
    worker.0.try_shutdown()?;
    assert_eq!(worker.0.status().phase, Phase::Stopped);
    worker.0.try_shutdown()?;
    #[cfg(unix)]
    {
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    // A fresh configured owner sees exactly the original files, with trustworthy
    // created=false provenance. It does not replay the first Create operation.
    let worker = NoDependents(Client::spawn(
        assert_cmd::cargo::cargo_bin!("photocatalog"),
        vec![],
    )?);
    eprintln!("filesystem fixture owned pid={}", worker.0.pid());
    request.operation = U64(2);
    request.session = LeaseId::new();
    request.mode = BootstrapMode::DesktopExisting;
    let reopened = worker
        .0
        .prepare_catalog(&request, &AtomicBool::new(false))?;
    assert_eq!(reopened.catalog.physical, bootstrap.catalog.physical);
    assert!(!reopened.catalog.created && !reopened.manifest.created);
    let path = reopened.catalog.path.to_path()?;
    fs::rename(&path, root.join("original-database"))?;
    fs::write(&path, b"replacement")?;
    assert!(
        worker
            .0
            .restore_status(&reopened.root_capability())
            .is_err()
    );
    worker
        .0
        .abandon_prepare(request.operation, &request.session)?;
    worker.0.try_shutdown()?;
    assert_eq!(fs::read(&path)?, b"replacement");
    Ok(())
}
