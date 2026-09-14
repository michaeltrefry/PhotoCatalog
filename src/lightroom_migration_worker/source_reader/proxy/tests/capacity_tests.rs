use super::*;
use crate::capacity_probes as probe;

#[test]
fn capacity_sql_raw_epochs_overlap_without_result_history() -> Result<()> {
    let baseline = probe::begin();
    let fixture = Fixture::new();
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let raw_bytes = vec![19; 65_536];
    let descriptor = raw_descriptor(&root, &raw_bytes)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let sql = sql(&fixture, cancel.clone())?;
    let sql_pid = sql.session.borrow().process.pid();
    let mut raw = session(
        Authority::Artifact {
            descriptor,
            limits: RawLimits {
                maximum_bytes: U64(100_000),
                open_deadline_ms: U64(10_000),
                chunk_deadline_ms: U64(5000),
                chunk_bytes: U64(4096),
            },
            protected: vec![],
        },
        cancel.clone(),
    )?;
    let raw_pid = raw.process.pid();
    for index in 0..16 {
        let first = sql.capture_manifest(fixture.revision())?;
        let second = sql.capture_manifest(fixture.revision())?;
        assert_eq!(first.revision_id, second.revision_id);
        probe::observe(
            probe::PENDING_MANIFEST,
            probe::manifest(&first) + probe::manifest(&second),
        );
        let Value::Chunk(bytes) = raw.query(Read::ArtifactChunk {
            offset: U64(index * 4096),
        })?
        else {
            anyhow::bail!("raw result");
        };
        assert_eq!(
            bytes,
            raw_bytes[index as usize * 4096..(index as usize + 1) * 4096]
        );
        assert!(matches!(raw.query(Read::ArtifactVerify)?, Value::Verified));
        drop((first, second, bytes));
    }
    cancel.store(true, Ordering::Release);
    assert!(sql.count(fixture.revision(), Collection::Rows).is_err());
    assert!(!can_write(&fixture.path));
    sql.session.borrow_mut().retire()?;
    raw.retire()?;
    assert!(sql.session.borrow_mut().process.try_reap()?.is_some());
    assert!(raw.process.try_reap()?.is_some());
    drop((sql, raw));
    assert!(can_write(&fixture.path));
    #[cfg(unix)]
    for pid in [sql_pid, raw_pid] {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    println!(
        "CAPACITY_SOURCE_PROCESSES sql_pid={sql_pid} raw_pid={raw_pid} queries=64 retained_manifests=2 raw_epochs=1 reaped=true"
    );
    assert!(probe::visits(probe::SOURCE_DECODED) >= 64);
    probe::report("sql-raw-epochs", baseline);
    Ok(())
}
