use super::*;
use crate::{
    lightroom::{migration_source::tests::Fixture, source::Source},
    storage_volume::NativePath,
};
use std::{fs, process::Command};
const FIXTURE: &str = "PHOTOCATALOG_CLOSED_SOURCE_FIXTURE";
const HELPER: &str =
    "lightroom_migration_worker::source_reader::proxy::tests::owned_source_fixture";

#[test]
fn owned_source_fixture() -> Result<()> {
    if std::env::var_os(FIXTURE).is_none() {
        return Ok(());
    }
    super::super::owner::serve(std::io::stdin(), std::io::stderr())?;
    std::process::exit(0);
}
fn epoch() -> Epoch {
    Epoch {
        guard: Guard {
            session: "session".into(),
            generation: "1".into(),
            operation: "operation".into(),
        },
        reader: "reader-1".into(),
    }
}
pub(super) fn session(authority: Authority, cancel: Arc<AtomicBool>) -> Result<Session> {
    session_with_deadline(authority, cancel, 10_000)
}
pub(super) fn session_with_deadline(
    authority: Authority,
    cancel: Arc<AtomicBool>,
    read_ms: u64,
) -> Result<Session> {
    let binding = authority.binding()?;
    let encoded = exact_json(&authority, AUTHORITY_BYTES, &cancel)?;
    let stop = Arc::new(Stop::default());
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", HELPER, "--nocapture"])
        .env(FIXTURE, "1");
    let process = Process::spawn_test_command(command, stop.clone())?;
    let budget = super::super::wire::Budget::from_authority(&authority)?;
    Session::admit(
        process,
        stop,
        epoch(),
        binding,
        encoded,
        cancel,
        15_000,
        read_ms,
        budget,
    )
}
pub(super) fn sql(fixture: &Fixture, cancel: Arc<AtomicBool>) -> Result<SqlReader> {
    let session = session(
        Authority::Sql {
            seal: fixture.seal.clone(),
            limits: ReadLimits::default().into(),
            protected: vec![],
        },
        cancel,
    )?;
    Ok(SqlReader {
        binding: session.binding.clone(),
        session: RefCell::new(session),
        seal: fixture.seal.clone(),
        chunk_bytes: ReadLimits::default().chunk_bytes,
    })
}
fn can_write(path: &Path) -> bool {
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        // A whole-file writer overlaps every admitted SQL/artifact barrier.
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) == 0 }
    }
    #[cfg(windows)]
    {
        fs2::FileExt::try_lock_exclusive(&file).is_ok()
    }
}
fn same<T: serde::Serialize>(a: T, b: T) {
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap()
    );
}
#[test]
fn remote_sql_full_method_parity_and_cancel_retain_lock_through_consumer_drain() -> Result<()> {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db| {
        db.execute("INSERT INTO entities VALUES(?,'image','Adobe_images',NULL,NULL,'{}')", [&revision]).unwrap();
        db.execute(r#"UPDATE rows SET key_json='[{"type":"Integer","value":9007199254740993}]' WHERE revision=?"#, [&revision]).unwrap();
    });
    let before = fs::read(&fixture.path)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let remote = sql(&fixture, cancel.clone())?;
    let pid = remote.session.borrow().process.pid();
    assert!(!can_write(&fixture.path));
    let local = fixture.open();
    same(remote.seal(), local.seal());
    assert_eq!(remote.binding_blake3(), local.binding_blake3());
    assert_eq!(remote.max_chunk_bytes(), local.max_chunk_bytes());
    let rev = fixture.revision();
    same(remote.capture_manifest(rev)?, local.capture_manifest(rev)?);
    same(
        remote.stable_source(rev, "lineage-selected:7")?,
        local.stable_source(rev, "lineage-selected:7")?,
    );
    same(
        remote.origin_packet_roster(rev, "lineage-selected:7", "embedded")?,
        local.origin_packet_roster(rev, "lineage-selected:7", "embedded")?,
    );
    for collection in [
        Collection::Captures,
        Collection::Rows,
        Collection::Entities,
        Collection::References,
        Collection::Paths,
        Collection::Packets,
        Collection::MetadataFacts,
        Collection::Issues,
        Collection::Tables,
        Collection::SchemaObjects,
    ] {
        same(
            remote.page(rev, collection, None, 100)?,
            local.page(rev, collection, None, 100)?,
        );
        assert_eq!(
            remote.count(rev, collection)?,
            local.count(rev, collection)?
        );
    }
    same(
        remote.image_links(rev, "image")?,
        local.image_links(rev, "image")?,
    );
    same(
        remote.resolve(rev, "missing", "rootFile", "AgLibraryFile")?,
        local.resolve(rev, "missing", "rootFile", "AgLibraryFile")?,
    );
    // Packet bytes use a raw descriptor; no JS number conversion or source reopen.
    let page = remote.page(rev, Collection::Packets, None, 1)?;
    if let crate::lightroom::migration_source::Field::Bytes(reference) =
        &page.records[0].fields["raw"]
    {
        same(
            remote.read_chunk(reference, 0, 4)?,
            local.read_chunk(reference, 0, 4)?,
        );
    } else {
        anyhow::bail!("expected packet byte reference");
    }
    drop(local);
    let mut destination = rusqlite::Connection::open_in_memory()?;
    let tx = destination.transaction()?;
    tx.execute_batch("CREATE TABLE held(value TEXT); INSERT INTO held VALUES('pending')")?;
    cancel.store(true, Ordering::Release);
    assert!(remote.count(rev, Collection::Rows).is_err());
    assert!(
        !can_write(&fixture.path),
        "cancel must retain source until consumer rollback"
    );
    drop(tx);
    drop(remote);
    assert!(can_write(&fixture.path));
    #[cfg(unix)]
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "source PID must be reaped"
    );
    assert_eq!(fs::read(&fixture.path)?, before);
    Ok(())
}
fn raw_descriptor(root: &Path, bytes: &[u8]) -> Result<ArtifactDescriptor> {
    let path = root.join("capture.bin");
    fs::write(&path, bytes)?;
    let source = Source::open(&path, u64::MAX)?;
    let identity = source.before.clone();
    drop(source);
    Ok(ArtifactDescriptor {
        protocol: 1,
        request: crate::catalog_migration::artifacts::ArtifactRequest {
            retained_capture_record: 1,
            member_index: 0,
            mapping: crate::catalog_migration::artifacts::ArtifactMapping {
                root: NativePath::from_path(root),
                relative: NativePath::from_path(Path::new("capture.bin")),
                copy_identity: identity.clone(),
            },
        },
        selected_input: "a".repeat(64),
        capture_revision: "b".repeat(64),
        manifest_blake3: "c".repeat(64),
        artifact: crate::lightroom::capture::Artifact {
            source: NativePath::WindowsWide(vec![0xd800]),
            role: "main".into(),
            relative: NativePath::WindowsWide(vec![0xd801]),
            stored: "raw/opaque".into(),
            revision: identity,
            blake3: crate::lightroom::digest(bytes),
        },
    })
}
#[test]
fn raw_reader_hash_once_chunks_and_locked_path_command_rejection_keep_custody() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let bytes = vec![255; 70_000];
    let descriptor = raw_descriptor(&root, &bytes)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut session = session(
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
        cancel,
    )?;
    assert!(!can_write(&root.join("capture.bin")));
    let mut restored = vec![];
    while restored.len() < bytes.len() {
        let Value::Chunk(chunk) = session.query(Read::ArtifactChunk {
            offset: U64(restored.len() as u64),
        })?
        else {
            anyhow::bail!("raw chunk");
        };
        restored.extend_from_slice(&chunk);
    }
    assert_eq!(restored, bytes);
    let chain = session.chain.clone();
    let completed = session.completed;
    // A second authority upload is rejected by the locked vocabulary. No
    // pathname opener is called; the exact current roster remains held.
    let until = Instant::now() + Duration::from_secs(3);
    session.send(
        Request::Open {
            epoch: session.epoch.clone(),
        },
        until,
    )?;
    assert!(session.receive(until, false).is_err());
    assert_eq!(session.chain, chain);
    assert_eq!(session.completed, completed);
    assert!(!can_write(&root.join("capture.bin")));
    session.retire()?;
    assert!(can_write(&root.join("capture.bin")));
    assert_eq!(fs::read(root.join("capture.bin"))?, bytes);
    Ok(())
}

#[cfg(unix)]
#[test]
fn observed_reader_death_rolls_back_before_commit_but_durable_commit_wins() -> Result<()> {
    use crate::{
        Catalog,
        catalog_writer::Writers,
        lightroom_migration_worker::{
            identity::Audit,
            lease::{DestinationLease, DestinationReview},
            source_reader::CommitHealth,
        },
    };
    for die_before_commit in [true, false] {
        let fixture = Fixture::new();
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("catalog");
        drop(Catalog::open(&root)?);
        let replacement = Catalog::open(&temp.path().join("replacement"))?;
        let root = root.canonicalize()?;
        fs::write(root.join(".lightroom-import.lock"), [])?;
        let remote = sql(&fixture, Arc::new(AtomicBool::new(false)))?;
        // Authenticate a real source result before the transaction begins.
        assert_eq!(remote.count(fixture.revision(), Collection::Rows)?, 1);
        let sources = CommitHealth::new(remote.health());
        let audit = Audit::new(Arc::new(AtomicBool::new(false)), vec![])?;
        let review = DestinationReview::existing(&NativePath::from_path(&root), None, &audit)?;
        let lease =
            DestinationLease::acquire(review, None, Instant::now() + Duration::from_secs(3))?;
        let mut wrapper =
            lease.open_current_with_sources(Arc::new(Writers::default()), sources.clone())?;
        // Safe Rust can extract Catalog through DerefMut. Its callback must
        // remain owned by the moved Connection after the old wrapper drops.
        let mut catalog = std::mem::replace(&mut *wrapper, replacement);
        drop(wrapper);
        catalog
            .db
            .execute_batch("CREATE TABLE source_commit_probe(value TEXT)")?;
        let tx = catalog.db.transaction()?;
        tx.execute(
            "INSERT INTO source_commit_probe VALUES('exact authenticated source result')",
            [],
        )?;
        let pid = remote.session.borrow().process.pid();
        if die_before_commit {
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
            let until = Instant::now() + Duration::from_secs(3);
            while !remote.health().failed() && Instant::now() < until {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                remote.health().failed(),
                "EOF must reach atomic precommit observation"
            );
            assert!(tx.commit().is_err());
        } else {
            tx.commit()?;
            assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
            let until = Instant::now() + Duration::from_secs(3);
            while !remote.health().failed() && Instant::now() < until {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(remote.health().failed());
        }
        let count: i64 =
            catalog
                .db
                .query_row("SELECT count(*) FROM source_commit_probe", [], |r| r.get(0))?;
        assert_eq!(count, i64::from(!die_before_commit));
        assert!(sources.failed());
        // An unchanged destination connection never silently re-admits source.
        assert!(
            catalog
                .db
                .execute("INSERT INTO source_commit_probe VALUES('must reject')", [])
                .is_err()
        );
        drop(catalog);
        drop(lease);
        drop(remote);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert!(can_write(&fixture.path));
    }
    Ok(())
}

#[test]
fn maximum_frames_and_escaped_results_keep_exact_bytes_and_tickets() -> Result<()> {
    let epoch = epoch();
    let request = Request::Authority {
        epoch: epoch.clone(),
        offset: U64(u64::MAX),
        bytes: vec![255; CHUNK_BYTES],
    };
    let mut encoded = vec![];
    crate::lightroom_migration_worker::protocol::write_frame(&mut encoded, &request)?;
    assert!(encoded.len() <= crate::lightroom_migration_worker::protocol::FRAME_BYTES + 4);
    let decoded: Request =
        crate::lightroom_migration_worker::protocol::read_frame(&mut encoded.as_slice())?;
    let Request::Authority { bytes, offset, .. } = decoded else {
        anyhow::bail!("frame kind");
    };
    assert_eq!(bytes, vec![255; CHUNK_BYTES]);
    assert_eq!(offset.0, u64::MAX);
    let value = Value::Chunk(vec![255; 1024 * 1024]);
    let payload = crate::lightroom::bounded_json(&value, RESULT_BYTES)?;
    assert!(payload.len() > crate::lightroom_migration_worker::protocol::FRAME_BYTES);
    let digest = crate::lightroom::digest(&payload);
    let mut restored = vec![];
    for (index, bytes) in payload.chunks(CHUNK_BYTES).enumerate() {
        let frame = Reply::Chunk {
            epoch: epoch.clone(),
            sequence: U64(9007199254740993),
            offset: U64((index * CHUNK_BYTES) as u64),
            bytes: bytes.to_vec(),
        };
        let mut wire = vec![];
        crate::lightroom_migration_worker::protocol::write_frame(&mut wire, &frame)?;
        let Reply::Chunk {
            bytes,
            sequence,
            offset,
            ..
        } = crate::lightroom_migration_worker::protocol::read_frame::<Reply>(&mut wire.as_slice())?
        else {
            anyhow::bail!("chunk kind");
        };
        assert_eq!(sequence.0, 9007199254740993);
        assert_eq!(offset.0, restored.len() as u64);
        restored.extend_from_slice(&bytes);
    }
    assert_eq!(restored, payload);
    assert_eq!(crate::lightroom::digest(&restored), digest);
    let a = next_chain(&"a".repeat(64), 9007199254740993, &"b".repeat(64), &digest)?;
    assert_ne!(
        a,
        next_chain(&"a".repeat(64), 9007199254740992, &"b".repeat(64), &digest)?
    );
    assert_ne!(
        a,
        next_chain(&"a".repeat(64), 9007199254740993, &"c".repeat(64), &digest)?
    );
    Ok(())
}

#[test]
fn authority_preserves_complete_u128_range_and_reports_open_failure() -> Result<()> {
    use serde::{Deserialize, Serialize};
    // Exact v12 representation: its internally tagged buffering path does not
    // implement deserialize_u128, including otherwise ordinary timestamps.
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "mode")]
    enum V12 {
        Sql {
            seal: InputSeal,
            limits: SqlLimits,
            protected: Vec<FileKey>,
        },
    }
    let fixture = Fixture::new();
    for nanos in [Some(1), Some(u64::MAX as u128), Some(u128::MAX), None] {
        let mut seal = fixture.seal.clone();
        seal.identity.modified_ns = nanos;
        let old = V12::Sql {
            seal: seal.clone(),
            limits: ReadLimits::default().into(),
            protected: vec![],
        };
        let old_bytes = serde_json::to_vec(&old)?;
        let old_result = serde_json::from_slice::<V12>(&old_bytes);
        if nanos.is_some() {
            assert!(
                old_result.is_err(),
                "v12 must demonstrate its rejected timestamp"
            );
        }
        let authority = Authority::Sql {
            seal: seal.clone(),
            limits: ReadLimits::default().into(),
            protected: vec![],
        };
        let bytes = crate::lightroom::bounded_json(&authority, AUTHORITY_BYTES)?;
        let decoded: Authority = serde_json::from_slice(&bytes)?;
        let Authority::Sql { seal: actual, .. } = decoded else {
            anyhow::bail!("SQL authority kind");
        };
        assert_eq!(actual.identity.modified_ns, nanos);
        assert_eq!(actual.binding_blake3()?, seal.binding_blake3()?);
    }
    let mut missing = fixture.seal.clone();
    missing.database = NativePath::from_path(&fixture.path.with_file_name("missing.sqlite3"));
    let error = session(
        Authority::Sql {
            seal: missing,
            limits: ReadLimits::default().into(),
            protected: vec![],
        },
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .context("missing source must reject")?;
    let detail = format!("{error:#}");
    assert!(
        detail.contains("source reader:"),
        "opening failure must be framed: {detail}"
    );
    assert!(
        !detail.contains("frame byte admission"),
        "must preserve original failure, not harness framing: {detail}"
    );
    Ok(())
}
