use super::*;
use crate::application::{self, Config};
use std::fs::{File, OpenOptions};
use std::time::Duration;

fn guard() -> Guard {
    Guard {
        session: "desktop-fixture".into(),
        generation: "a".repeat(64),
        operation: "operation-one".into(),
    }
}
fn request(action: Action) -> Request {
    Request {
        guard: guard(),
        action,
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "migration relay fixture deadline");
        thread::sleep(Duration::from_millis(2));
    }
}
fn config() -> anyhow::Result<Config> {
    Ok(Config {
        worker_executable: std::env::current_exe()?,
        cache_root: None,
        original_roots: vec![],
        preview_policy: Default::default(),
        preview_limits: Default::default(),
        limits: Default::default(),
        import_checkpoint: None,
    })
}
fn actor() -> anyhow::Result<(application::Bridge, Actor)> {
    let bridge = application::tests::disconnected();
    let actor = Actor::new(config()?, bridge.0.shared.clone());
    Ok((bridge, actor))
}
struct ManagedF(Arc<crate::filesystem_worker::client::Client>);
impl Drop for ManagedF {
    fn drop(&mut self) {
        // F validates that no retained root remains before acknowledging close.
        // A failed assertion cannot authorize killing a dependent-owning F.
        if let Err(error) = self.0.try_shutdown() {
            std::mem::forget(self.0.clone());
            if !thread::panicking() {
                panic!("managed fixture F drain unresolved: {error:#}");
            }
        }
    }
}
fn managed_actor(base: &std::path::Path) -> anyhow::Result<(ManagedF, Actor)> {
    let filesystem = Arc::new(crate::filesystem_worker::client::migration_fixture(base)?);
    let (_bridge, mut actor) = actor()?;
    actor.managed = Some(application::ManagedCatalogConfig {
        filesystem: filesystem.clone(),
    });
    Ok((ManagedF(filesystem), actor))
}
fn open_actor(root: &std::path::Path) -> anyhow::Result<(ManagedF, Actor, Request)> {
    let (filesystem, mut actor) = managed_actor(root.parent().unwrap())?;
    actor.open_path(NativePath::from_path(root), true, &Cancellation::default())?;
    let open = actor.open.as_ref().unwrap();
    let acquire = request(Action::AcquireTarget {
        catalog: Some(open.token.clone()),
        destination: NativePath::from_path(root),
        expected: Some(managed_pin(open)?),
    });
    Ok((filesystem, actor, acquire))
}
fn write(kind: WriteKind, sequence: u64, lock: Option<FileKey>) -> Request {
    let guard = guard();
    let target = guard.generation.clone();
    let request_digest = write_digest(&guard, U64(sequence), kind, &target, lock.as_ref()).unwrap();
    Request {
        guard,
        action: Action::AcquireWrite {
            sequence: U64(sequence),
            kind,
            target,
            lock,
            request_digest,
        },
    }
}
fn release(write: &Request) -> Request {
    let Action::AcquireWrite {
        sequence,
        kind,
        request_digest,
        ..
    } = &write.action
    else {
        panic!()
    };
    Request {
        guard: write.guard.clone(),
        action: Action::ReleaseWrite {
            sequence: *sequence,
            kind: *kind,
            request_digest: request_digest.clone(),
        },
    }
}
fn drain(actor: &mut Actor) {
    wait(|| {
        actor
            .migration_action(request(Action::DrainOperation))
            .unwrap()
            .phase
            == Phase::Drained
    });
}

#[test]
fn lm_desktop_relay_permit_is_thread_affine_and_cancel_does_not_release_held() -> anyhow::Result<()>
{
    let writers = Arc::new(Writers::default());
    let mut owner = PermitOwner::start(writers.clone(), || Ok(()))?;
    wait(|| owner.snapshot().0 == Phase::Held);
    owner.cancel();
    assert_eq!(owner.snapshot().0, Phase::Held);
    assert!(
        writers
            .enter_cancellable(
                Priority::Foreground,
                &AtomicBool::new(false),
                Some(Instant::now() + Duration::from_millis(20))
            )
            .is_err()
    );
    owner.release();
    wait(|| owner.join_ready().unwrap());
    let permit = writers.enter(Priority::Foreground)?;
    drop(permit);
    assert_eq!(owner.snapshot().0, Phase::Released);
    assert!(owner.join_ready()?);
    Ok(())
}

#[test]
fn lm_desktop_relay_pending_cancel_and_checked_join_preserve_actual_writer() -> anyhow::Result<()> {
    let writers = Arc::new(Writers::default());
    let held = writers.enter(Priority::Foreground)?;
    let mut owner = PermitOwner::start(writers.clone(), || Ok(()))?;
    writers.wait_until_queued(0, 1);
    assert_eq!(owner.snapshot().0, Phase::Attempted);
    owner.cancel();
    wait(|| owner.join_ready().unwrap());
    let (phase, failure) = owner.snapshot();
    assert_eq!(phase, Phase::Failed);
    assert!(matches!(failure.unwrap().code, ErrorCode::Canceled));
    drop(held);
    drop(writers.enter(Priority::Foreground)?);
    Ok(())
}

#[test]
fn lm_desktop_relay_duplicate_acquire_lost_grant_release_status_and_close_retention()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (_bridge, mut actor, acquire) = open_actor(&temp.path().canonicalize()?.join("catalog"))?;
    let first = actor.migration_action(acquire.clone())?;
    assert_eq!(first.phase, Phase::Target);
    assert_eq!(
        actor.migration_action(acquire)?.destination,
        first.destination
    );
    // Existing current catalogs also need a Bootstrap grant to create their
    // first physical import lock; Bootstrap must not recreate the catalog.
    let acquire_write = write(WriteKind::Bootstrap, 1, None);
    actor.migration_action(acquire_write.clone())?; // Simulate lost Grant reply.
    wait(|| actor.migration_action(acquire_write.clone()).unwrap().phase == Phase::Held);
    let catalog = actor.open.as_ref().unwrap().token.clone();
    assert_eq!(
        actor.open.as_ref().unwrap().catalog.root,
        temp.path().canonicalize()?.join("catalog")
    );
    assert!(matches!(
        actor
            .command(
                application::Request::Close { catalog },
                &Cancellation::default()
            )
            .unwrap_err()
            .code,
        ErrorCode::Busy
    ));
    assert!(actor.migration.held());
    assert_eq!(
        actor.migration_action(request(Action::Status))?.phase,
        Phase::Held
    );
    assert!(matches!(
        actor
            .migration_action(write(WriteKind::Bootstrap, 2, None))
            .unwrap_err()
            .code,
        ErrorCode::Busy
    ));
    actor.migration_action(release(&acquire_write))?; // Simulate lost Release reply.
    wait(|| {
        actor
            .migration_action(release(&acquire_write))
            .unwrap()
            .phase
            == Phase::Released
    });
    drain(&mut actor);
    assert!(!actor.migration.held());
    assert_eq!(
        actor.migration_action(release(&acquire_write))?.phase,
        Phase::Drained
    );
    assert_eq!(
        actor
            .migration_action(request(Action::DrainOperation))?
            .phase,
        Phase::Drained
    );
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_exact_target_and_write_digest_refuse_changed_authority() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (_bridge, mut actor, acquire) = open_actor(&temp.path().canonicalize()?.join("catalog"))?;
    let mut wrong = acquire.clone();
    let Action::AcquireTarget { catalog, .. } = &mut wrong.action else {
        panic!()
    };
    *catalog = Some("stale".into());
    assert!(matches!(
        actor.migration_action(wrong).unwrap_err().code,
        ErrorCode::StaleSession
    ));
    assert!(!actor.migration.held());
    actor.migration_action(acquire)?;
    let mut altered = write(WriteKind::Bootstrap, 1, None);
    let Action::AcquireWrite { request_digest, .. } = &mut altered.action else {
        panic!()
    };
    *request_digest = "f".repeat(64);
    assert!(matches!(
        actor.migration_action(altered).unwrap_err().code,
        ErrorCode::InvalidRequest
    ));
    assert!(actor.migration.target.as_ref().unwrap().attempt.is_none());
    let mut wrong_operation = request(Action::Cancel);
    wrong_operation.guard.operation = "different".into();
    assert!(matches!(
        actor.migration_action(wrong_operation).unwrap_err().code,
        ErrorCode::StaleSession
    ));
    assert!(!actor.migration.target.as_ref().unwrap().canceled);
    drain(&mut actor);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_absent_destination_creates_only_after_recorded_bootstrap() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_bridge, mut actor) = managed_actor(&temp.path().canonicalize()?)?;
    let acquire = request(Action::AcquireTarget {
        catalog: None,
        destination: NativePath::from_path(&root),
        expected: None,
    });
    actor.migration_action(acquire.clone())?;
    assert!(!root.exists());
    assert!(actor.open.is_none());
    let create = write(WriteKind::Bootstrap, 1, None);
    actor.migration_action(create.clone())?;
    wait(|| actor.migration_action(create.clone()).unwrap().phase == Phase::Held);
    assert!(root.join("catalog.sqlite3").is_file());
    let token = actor.open.as_ref().unwrap().token.clone();
    assert_eq!(
        actor.migration_action(acquire)?.catalog.as_deref(),
        Some(token.as_str())
    );
    actor.migration_action(release(&create))?;
    drain(&mut actor);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_prospective_replacement_and_cancel_refuse_creation() -> anyhow::Result<()> {
    for cancel_first in [false, true] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("catalog");
        let (_bridge, mut actor) = managed_actor(&temp.path().canonicalize()?)?;
        actor.migration_action(request(Action::AcquireTarget {
            catalog: None,
            destination: NativePath::from_path(&root),
            expected: None,
        }))?;
        if cancel_first {
            actor.migration_action(request(Action::Cancel))?;
            assert!(matches!(
                actor
                    .migration_action(write(WriteKind::Bootstrap, 1, None))
                    .unwrap_err()
                    .code,
                ErrorCode::Canceled
            ));
            assert!(!root.exists());
        } else {
            std::fs::create_dir(&root)?;
            std::fs::write(root.join("unowned"), b"must remain unchanged")?;
            assert_eq!(
                actor
                    .migration_action(write(WriteKind::Bootstrap, 1, None))?
                    .phase,
                Phase::Failed
            );
            assert_eq!(
                std::fs::read(root.join("unowned"))?,
                b"must remain unchanged"
            );
            assert!(!root.join("catalog.sqlite3").exists());
        }
        drain(&mut actor);
        actor.close()?;
    }
    Ok(())
}

#[test]
fn lm_desktop_relay_recovery_lane_survives_desktop_shutdown_and_data_backlog() -> anyhow::Result<()>
{
    let shared = super::super::tests::shared(1024);
    let client = Client::new(&shared);
    shared.stop();
    let _pending = client.submit(request(Action::Status))?;
    let message = super::super::process::next_outgoing(&shared, &mut None).unwrap();
    assert_eq!(message.kind, super::super::wire::Kind::MigrationAdmission);
    let decoded: Request = serde_json::from_slice(&message.bytes)?;
    assert!(matches!(decoded.action, Action::Status));
    shared.complete_failure();
    assert!(matches!(
        _pending.receiver.recv()?,
        Reply::Error(BridgeError {
            code: ErrorCode::Closed,
            ..
        })
    ));
    Ok(())
}

#[test]
fn lm_desktop_relay_wire_is_private_bounded_and_strict() -> anyhow::Result<()> {
    let request = request(Action::Status);
    let encoded = serde_json::to_vec(&request)?;
    assert!(serde_json::from_slice::<application::Request>(&encoded).is_err());
    let mut value = serde_json::to_value(&request)?;
    value["unreviewed"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Request>(value).is_err());
    let maximum = request_with_phase("x".repeat(256));
    assert!(maximum.validate().is_ok());
    let too_large = request_with_phase("x".repeat(257));
    assert!(too_large.validate().is_err());
    assert_ne!(super::super::wire::build_identity(), "");
    Ok(())
}
fn request_with_phase(phase: String) -> Request {
    request(Action::Progress {
        phase,
        completed: U64(0),
        total: None,
    })
}

#[test]
fn lm_desktop_relay_tombstone_rejects_release_kind_in_both_directions() -> anyhow::Result<()> {
    for kind in [WriteKind::Bootstrap, WriteKind::Catalog] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("catalog");
        let (_bridge, mut actor, acquire) = open_actor(&root)?;
        actor.migration_action(acquire)?;
        let lock = if kind == WriteKind::Catalog {
            let path = root.join(".lightroom-import.lock");
            std::fs::write(&path, b"exact lock")?;
            Some(FileKey::of(&File::open(path)?)?)
        } else {
            None
        };
        let write = write(kind, 1, lock);
        actor.migration_action(write.clone())?;
        wait(|| {
            actor
                .migration_action(request(Action::Status))
                .unwrap()
                .phase
                == Phase::Held
        });
        drain(&mut actor);
        let mut changed = release(&write);
        let Action::ReleaseWrite {
            kind: changed_kind, ..
        } = &mut changed.action
        else {
            panic!()
        };
        *changed_kind = if kind == WriteKind::Catalog {
            WriteKind::Bootstrap
        } else {
            WriteKind::Catalog
        };
        assert!(matches!(
            actor.migration_action(changed).unwrap_err().code,
            ErrorCode::InvalidRequest
        ));
        assert_eq!(
            actor.migration_action(release(&write))?.phase,
            Phase::Drained
        );
        actor.close()?;
    }
    Ok(())
}

#[test]
fn lm_desktop_relay_nonregular_and_link_lock_objects_fail_before_writer_wait() -> anyhow::Result<()>
{
    for shape in ["directory", "symlink", "fifo"] {
        #[cfg(not(unix))]
        if shape != "directory" {
            continue;
        }
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?.join("catalog");
        let (_bridge, mut actor, acquire) = open_actor(&root)?;
        actor.migration_action(acquire)?;
        let path = root.join(".lightroom-import.lock");
        match shape {
            "directory" => std::fs::create_dir(&path)?,
            #[cfg(unix)]
            "symlink" => {
                let target = root.join("other-lock");
                std::fs::write(&target, b"must not follow")?;
                std::os::unix::fs::symlink(target, &path)?;
            }
            #[cfg(unix)]
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        // Supply the real object identity: rejection must be based on type,
        // links and nonblocking admission, not an intentionally wrong key.
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0200_0000).share_mode(1 | 2 | 4);
        }
        let key = FileKey::of(&options.open(&path)?)?;
        let before = Instant::now();
        let snapshot = actor.migration_action(write(WriteKind::Catalog, 1, Some(key)))?;
        assert_eq!(snapshot.phase, Phase::Failed, "{shape}");
        assert!(snapshot.failure.is_some());
        assert!(
            before.elapsed() < Duration::from_secs(2),
            "{shape} blocked Actor"
        );
        assert!(
            actor
                .migration
                .target
                .as_ref()
                .unwrap()
                .attempt
                .as_ref()
                .unwrap()
                .owner
                .is_none()
        );
        drain(&mut actor);
        actor.close()?;
    }
    Ok(())
}

#[test]
fn lm_desktop_relay_lock_identity_is_retained_and_rechecked_after_actual_writer_wait()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_bridge, mut actor, acquire) = open_actor(&root)?;
    actor.migration_action(acquire)?;
    let path = root.join(".lightroom-import.lock");
    std::fs::write(&path, b"original lock")?;
    let key = FileKey::of(&File::open(&path)?)?;
    let writers = actor.open.as_ref().unwrap().catalog.writers.clone();
    let held = writers.enter(Priority::Foreground)?;
    actor.migration_action(write(WriteKind::Catalog, 1, Some(key)))?;
    writers.wait_until_queued(0, 1);
    std::fs::rename(&path, root.join("old-lock"))?;
    std::fs::write(&path, b"replacement lock")?;
    drop(held);
    wait(|| {
        actor
            .migration_action(request(Action::Status))
            .unwrap()
            .phase
            == Phase::Failed
    });
    drain(&mut actor);
    drop(writers.enter(Priority::Foreground)?);
    actor.close()?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn lm_desktop_relay_bootstrap_rejects_linked_ancestor_after_actual_writer_wait()
-> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let ancestor = base.join("ancestor");
    let moved = base.join("moved-ancestor");
    std::fs::create_dir(&ancestor)?;
    let root = ancestor.join("catalog");
    let (_filesystem, mut actor, acquire) = open_actor(&root)?;
    actor.migration_action(acquire)?;
    let root_before = std::fs::metadata(&root)?;
    let database_before = std::fs::metadata(root.join("catalog.sqlite3"))?;
    let writers = actor.open.as_ref().unwrap().catalog.writers.clone();
    let held = writers.enter(Priority::Foreground)?;
    assert_eq!(
        actor
            .migration_action(write(WriteKind::Bootstrap, 1, None))?
            .phase,
        Phase::Attempted
    );
    writers.wait_until_queued(0, 1);
    std::fs::rename(&ancestor, &moved)?;
    std::os::unix::fs::symlink(&moved, &ancestor)?;
    // The linked spelling still reaches exactly the admitted objects. An
    // identity-only check would grant this Bootstrap, which carries no lock.
    let root_after = std::fs::metadata(&root)?;
    let database_after = std::fs::metadata(root.join("catalog.sqlite3"))?;
    let same_objects = (root_before.dev(), root_before.ino())
        == (root_after.dev(), root_after.ino())
        && (database_before.dev(), database_before.ino())
            == (database_after.dev(), database_after.ino());
    drop(held);
    let mut snapshot = actor.migration_action(request(Action::Status))?;
    wait(|| {
        snapshot = actor.migration_action(request(Action::Status)).unwrap();
        snapshot.phase != Phase::Attempted
    });
    // Restore the admitted spelling before explicit owner drain and teardown,
    // including when the observed phase would fail the regression assertions.
    std::fs::remove_file(&ancestor)?;
    std::fs::rename(&moved, &ancestor)?;
    assert!(same_objects);
    assert_eq!(snapshot.phase, Phase::Failed);
    assert!(snapshot.failure.is_some());
    drain(&mut actor);
    assert!(actor.migration.target.is_none());
    drop(writers.enter(Priority::Foreground)?);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_shutdown_rejects_queued_and_new_authority_but_keeps_recovery()
-> anyhow::Result<()> {
    let shared = super::super::tests::shared(1024);
    let client = Client::new(&shared);
    let pending = client.submit(write(WriteKind::Bootstrap, 1, None))?;
    shared.stop();
    let path = NativePath::from_path(&std::env::current_dir()?.join("must-not-create"));
    assert!(matches!(
        client
            .submit(request(Action::AcquireTarget {
                catalog: None,
                destination: path,
                expected: None
            }))
            .err()
            .unwrap()
            .code,
        ErrorCode::Closed
    ));
    assert!(matches!(
        pending.receiver.recv_timeout(Duration::from_secs(1))?,
        Reply::Refused(BridgeError {
            code: ErrorCode::Closed,
            ..
        })
    ));
    assert!(matches!(
        client
            .submit(write(WriteKind::Bootstrap, 2, None))
            .err()
            .unwrap()
            .code,
        ErrorCode::Closed
    ));
    for action in [
        Action::Status,
        Action::Cancel,
        release(&write(WriteKind::Bootstrap, 1, None)).action,
        Action::DrainOperation,
    ] {
        let pending = client.submit(request(action))?;
        let outgoing = super::super::process::next_outgoing(&shared, &mut None).unwrap();
        assert_eq!(outgoing.kind, super::super::wire::Kind::MigrationAdmission);
        assert!(
            serde_json::from_slice::<Request>(&outgoing.bytes)?
                .action
                .recovery()
        );
        drop(pending);
    }
    shared.complete_failure();
    Ok(())
}

#[test]
fn lm_desktop_relay_refusal_formatting_and_snapshot_encoding_are_bounded() -> anyhow::Result<()> {
    struct Large<'a>(&'a std::cell::Cell<usize>);
    impl std::fmt::Display for Large<'_> {
        fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for _ in 0..1_000_000 {
                self.0.set(self.0.get() + 1);
                out.write_str("\0é")?;
            }
            Ok(())
        }
    }
    let count = std::cell::Cell::new(0);
    let failure = native(Large(&count));
    assert!(failure.message.len() <= FAILURE_BYTES);
    assert!(
        count.get() <= FAILURE_BYTES / 3 + 1,
        "formatter built full error"
    );
    assert!(failure.message.capacity() <= 2 * FAILURE_BYTES);
    let mut failed_owner = PermitOwner::start(Arc::new(Writers::default()), || {
        Err(error(ErrorCode::Native, "é".repeat(FAILURE_BYTES * 4)))
    })?;
    wait(|| failed_owner.join_ready().unwrap());
    let (phase, stored) = failed_owner.snapshot();
    assert_eq!(phase, Phase::Failed);
    let stored = stored.unwrap();
    assert_eq!(stored.message.len(), FAILURE_BYTES);
    assert_eq!(stored.message.capacity(), FAILURE_BYTES);
    struct LargeSequence<'a>(&'a std::cell::Cell<usize>);
    impl Serialize for LargeSequence<'_> {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(Some(1_000_000))?;
            for _ in 0..1_000_000 {
                self.0.set(self.0.get() + 1);
                sequence.serialize_element("\0")?;
            }
            sequence.end()
        }
    }
    let serialized = std::cell::Cell::new(0);
    assert!(encode(&LargeSequence(&serialized)).is_err());
    assert!(
        serialized.get() < super::super::wire::CHUNK,
        "encoder serialized the complete escaped value"
    );
    let (_bridge, mut actor) = actor()?;
    let mut snapshot = actor.migration_action(request(Action::Status))?;
    snapshot.guard.session = "s".repeat(64);
    snapshot.guard.operation = "o".repeat(64);
    let expected_guard = snapshot.guard.clone();
    snapshot.phase = Phase::Held;
    snapshot.sequence = Some(U64(u64::MAX));
    snapshot.write_kind = Some(WriteKind::Catalog);
    snapshot.request_digest = Some("b".repeat(64));
    snapshot.failure = Some(failure);
    snapshot.destination = Some(DestinationPin {
        root: NativePath::UnixBytes(vec![0; super::super::wire::CHUNK]),
        root_key: FileKey {
            volume: U64(u64::MAX),
            index: U64(u64::MAX),
        },
        database_key: FileKey {
            volume: U64(u64::MAX),
            index: U64(u64::MAX),
        },
        schema: application::I64(i64::MAX),
    });
    let expected_pin = snapshot.destination.clone();
    let message = Reply::Ok(snapshot).message(7, 1024 * 1024);
    assert!(message.bytes.len() > super::super::wire::CHUNK);
    let Reply::Ok(decoded) = serde_json::from_slice::<Reply>(&message.bytes)? else {
        panic!("oversize detail lost operation identity")
    };
    assert_eq!(decoded.guard, expected_guard);
    assert_eq!(decoded.phase, Phase::Held);
    assert_eq!(decoded.sequence, Some(U64(u64::MAX)));
    assert_eq!(decoded.write_kind, Some(WriteKind::Catalog));
    assert_eq!(
        decoded.request_digest.as_deref(),
        Some("b".repeat(64).as_str())
    );
    assert_eq!(decoded.destination, expected_pin);
    assert!(matches!(decoded.failure.unwrap().code, ErrorCode::Native));
    let mut sink = FrameSink {
        bytes: Vec::with_capacity(super::super::wire::CHUNK),
        limit: super::super::wire::CHUNK,
    };
    use std::io::Write;
    sink.write_all(&vec![0; super::super::wire::CHUNK])?;
    assert!(sink.write_all(b"x").is_err());
    assert_eq!(sink.bytes.len(), super::super::wire::CHUNK);
    assert_eq!(sink.bytes.capacity(), super::super::wire::CHUNK);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_active_transport_cancel_reaches_managed_create_and_retains_cleanup()
-> anyhow::Result<()> {
    use crate::catalog_session::{
        CatalogBootstrap, CatalogFilesystem, ConfirmSqlAdmission, LeaseId, PrepareCatalog,
        RootCapability, SqlAdmissionConfirmed,
    };
    struct CanceledPrepare {
        entered: mpsc::SyncSender<()>,
        observed: AtomicBool,
        abandon: std::sync::atomic::AtomicUsize,
    }
    impl CatalogFilesystem for CanceledPrepare {
        fn prepare_catalog(
            &self,
            request: &PrepareCatalog,
            cancel: &AtomicBool,
        ) -> anyhow::Result<CatalogBootstrap> {
            assert_eq!(
                request.mode,
                crate::catalog_session::BootstrapMode::DesktopCreate
            );
            assert!(!cancel.load(Ordering::Acquire));
            self.entered.send(())?;
            let until = Instant::now() + Duration::from_secs(3);
            while !cancel.load(Ordering::Acquire) && Instant::now() < until {
                thread::sleep(Duration::from_millis(2));
            }
            self.observed
                .store(cancel.load(Ordering::Acquire), Ordering::Release);
            anyhow::bail!("managed Prepare canceled before SQL admission")
        }
        fn abandon_prepare(&self, _: U64, _: &LeaseId) -> anyhow::Result<()> {
            if self.abandon.fetch_add(1, Ordering::AcqRel) == 0 {
                anyhow::bail!("injected unresolved Prepare cleanup")
            }
            Ok(())
        }
        fn confirm_sql_admission(
            &self,
            _: &ConfirmSqlAdmission,
            _: &AtomicBool,
        ) -> anyhow::Result<SqlAdmissionConfirmed> {
            panic!("canceled Prepare must not open SQL")
        }
        fn restore_status(
            &self,
            _: &RootCapability,
        ) -> anyhow::Result<Option<crate::catalog_backup::RestoreStatus>> {
            panic!("canceled Prepare must not inspect catalog")
        }
        fn resume_restored_jobs(
            &self,
            _: &RootCapability,
            _: &str,
            _: bool,
        ) -> anyhow::Result<crate::catalog_backup::RestoreStatus> {
            unreachable!()
        }
        fn release_root(&self, _: &RootCapability) -> anyhow::Result<()> {
            panic!("Prepare cleanup must use its retained admission identity")
        }
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (entered, reached) = mpsc::sync_channel(1);
    let filesystem = Arc::new(CanceledPrepare {
        entered,
        observed: AtomicBool::new(false),
        abandon: Default::default(),
    });
    let bridge = Bridge::spawn_managed(
        config()?,
        application::ManagedCatalogConfig {
            filesystem: filesystem.clone(),
        },
    )?;
    let target = bridge.migration_admission(request(Action::AcquireTarget {
        catalog: None,
        destination: NativePath::from_path(&root),
        expected: None,
    }))?;
    assert!(matches!(
        target.receiver.recv_timeout(Duration::from_secs(3))?,
        Reply::Ok(_)
    ));
    let bootstrap = bridge.migration_admission(write(WriteKind::Bootstrap, 1, None))?;
    reached.recv_timeout(Duration::from_secs(3))?;
    bootstrap.cancel.cancel(); // The same cancellation registered by C transport.
    let Reply::Ok(failed) = bootstrap.receiver.recv_timeout(Duration::from_secs(5))? else {
        panic!()
    };
    assert_eq!(failed.phase, Phase::Failed);
    assert!(filesystem.observed.load(Ordering::Acquire));
    assert_eq!(filesystem.abandon.load(Ordering::Acquire), 1);
    assert!(!root.exists());
    assert!(
        bridge.try_shutdown().is_err(),
        "migration hold must retain failed Prepare cleanup"
    );
    let drained = bridge.migration_admission(request(Action::DrainOperation))?;
    assert!(matches!(
        drained.receiver.recv_timeout(Duration::from_secs(3))?,
        Reply::Ok(Snapshot {
            phase: Phase::Drained,
            ..
        })
    ));
    bridge.try_shutdown()?;
    assert_eq!(filesystem.abandon.load(Ordering::Acquire), 2);
    Ok(())
}

#[test]
fn lm_desktop_relay_legacy_route_refuses_target_without_filesystem_authority() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_bridge, mut actor) = actor()?;
    let failure = actor
        .migration_action(request(Action::AcquireTarget {
            catalog: None,
            destination: NativePath::from_path(&root),
            expected: None,
        }))
        .unwrap_err();
    assert!(matches!(failure.code, ErrorCode::InvalidRequest));
    assert!(!root.exists());
    assert!(!actor.migration.held());
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_physical_identity_uses_matching_native_64_bit_contract() -> anyhow::Result<()> {
    let key = FileKey {
        volume: U64(u32::MAX.into()),
        index: U64(u64::MAX),
    };
    let physical = physical_key(&key)?;
    assert_eq!(file_key(physical)?, key);
    assert_eq!(
        serde_json::from_slice::<PhysicalObjectId>(&serde_json::to_vec(&physical)?)?,
        physical
    );
    #[cfg(unix)]
    assert!(
        file_key(PhysicalObjectId::Windows {
            volume_serial: U64(1),
            file_index: U64(u64::MAX)
        })
        .is_err()
    );
    #[cfg(windows)]
    {
        assert!(
            file_key(PhysicalObjectId::Unix {
                device: U64(1),
                inode: U64(u64::MAX)
            })
            .is_err()
        );
        assert!(
            physical_key(&FileKey {
                volume: U64(u64::MAX),
                index: U64(1)
            })
            .is_err()
        );
    }
    // FILE_ID_INFO's 128-bit export identity has a different wire vocabulary.
    // It cannot be deserialized or silently truncated into LM's FileKey.
    let export = crate::catalog_session::ExportObjectKey {
        volume: U64(1),
        object: u128::MAX.to_string(),
    };
    assert!(serde_json::from_value::<FileKey>(serde_json::to_value(&export)?).is_err());
    assert!(serde_json::from_value::<PhysicalObjectId>(serde_json::to_value(&export)?).is_err());
    Ok(())
}

#[test]
fn lm_desktop_relay_f_rejects_protected_aliases_without_releasing_c_sql_locks() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_filesystem, mut actor, acquire) = open_actor(&root)?;
    let open = actor.open.as_ref().unwrap();
    let bootstrap = open.managed.as_ref().unwrap().bootstrap.clone();
    let authority = open.catalog.session.clone();
    open.catalog.db.execute_batch("BEGIN IMMEDIATE")?;
    let database = root.join("catalog.sqlite3");
    assert!(!super::super::filesystem_tests::contender(&database)?);
    actor.migration_action(acquire)?;
    let lock_path = root.join(".lightroom-import.lock");
    for pin in [&bootstrap.catalog, &bootstrap.manifest] {
        std::fs::hard_link(pin.path.to_path()?, &lock_path)?;
        // Expected identity comes from existing managed bootstrap, not a raw
        // File opened in C (which would invalidate this SQL-lock regression).
        assert!(
            authority
                .verify_migration_identity(Some(pin.physical), &AtomicBool::new(false))
                .is_err()
        );
        assert!(
            !super::super::filesystem_tests::contender(&database)?,
            "F alias refusal disturbed C SQLite write lock"
        );
        std::fs::remove_file(&lock_path)?;
    }
    for _ in 0..3 {
        authority.verify_migration_identity(None, &AtomicBool::new(false))?;
        assert!(!super::super::filesystem_tests::contender(&database)?);
    }
    actor
        .open
        .as_ref()
        .unwrap()
        .catalog
        .db
        .execute_batch("ROLLBACK")?;
    assert!(super::super::filesystem_tests::contender(&database)?);
    drain(&mut actor);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_identity_fact_binds_root_lock_and_existing_wire_bounds() -> anyhow::Result<()> {
    use crate::catalog_session::{
        CatalogFilesystem, MigrationIdentityReply, MigrationIdentityRequest,
    };
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (filesystem, mut actor, _) = open_actor(&root)?;
    let capability = actor
        .open
        .as_ref()
        .unwrap()
        .managed
        .as_ref()
        .unwrap()
        .bootstrap
        .root_capability();
    let request = MigrationIdentityRequest {
        root: capability,
        lock: None,
    };
    let reply = filesystem
        .0
        .migration_identity(&request, &AtomicBool::new(false))?;
    reply.validate_for(&request)?;
    let mut changed = reply.clone();
    changed.lock = Some(physical_key(&FileKey {
        volume: U64(1),
        index: U64(2),
    })?);
    assert!(changed.validate_for(&request).is_err());
    let mut changed = reply.clone();
    changed.root.session = crate::catalog_session::LeaseId::new();
    assert!(changed.validate_for(&request).is_err());
    let encoded = serde_json::to_vec(&reply)?;
    assert!(encoded.len() < crate::filesystem_worker::wire::MESSAGE_BYTES);
    serde_json::from_slice::<MigrationIdentityReply>(&encoded)?.validate_for(&request)?;
    let mut invalid = serde_json::to_value(&reply)?;
    invalid["export_object_key"] = serde_json::json!({"object":u128::MAX.to_string()});
    assert!(serde_json::from_value::<MigrationIdentityReply>(invalid).is_err());
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_full_native_paths_roundtrip_at_exact_message_boundaries() -> anyhow::Result<()>
{
    let limit = 1024 * 1024;
    for path in [
        NativePath::UnixBytes({
            let mut v = vec![255; crate::catalog_session::PATH_UNITS];
            v[0] = b'/';
            v
        }),
        NativePath::WindowsWide({
            let mut v = vec![65535; crate::catalog_session::PATH_UNITS];
            v[..3].copy_from_slice(&[67, 58, 92]);
            v
        }),
    ] {
        let pin = DestinationPin {
            root: path.clone(),
            root_key: FileKey {
                volume: U64(u64::MAX),
                index: U64(u64::MAX),
            },
            database_key: FileKey {
                volume: U64(7),
                index: U64(9),
            },
            schema: application::I64(1),
        };
        let request = request(Action::AcquireTarget {
            catalog: Some("catalog".into()),
            destination: path.clone(),
            expected: Some(pin.clone()),
        });
        request.validate()?;
        let bytes = encode_bounded(&request, limit)?;
        assert!(bytes.len() > super::super::wire::CHUNK);
        assert_eq!(bytes.capacity(), bytes.len());
        assert_eq!(encode_bounded(&request, bytes.len())?, bytes);
        assert!(encode_bounded(&request, bytes.len() - 1).is_err());
        let decoded: Request = serde_json::from_slice(&bytes)?;
        let Action::AcquireTarget {
            destination,
            expected,
            ..
        } = decoded.action
        else {
            panic!()
        };
        assert_eq!(destination, path);
        assert_eq!(expected, Some(pin.clone()));
        let snapshot = Snapshot {
            guard: guard(),
            phase: Phase::Held,
            catalog: Some("catalog".into()),
            destination: Some(pin.clone()),
            sequence: Some(U64(1)),
            request_digest: Some("a".repeat(64)),
            write_kind: Some(WriteKind::Bootstrap),
            cancel_requested: false,
            progress: None,
            failure: None,
        };
        let reply = Reply::Ok(snapshot);
        reply.validate()?;
        let length = encoded_len(&reply, limit)?;
        let message = reply.clone().message(1, length);
        assert_eq!(message.bytes.len(), length);
        let Reply::Ok(decoded) = serde_json::from_slice::<Reply>(&message.bytes)? else {
            panic!("complete pin lost")
        };
        assert_eq!(decoded.destination, Some(pin));
        assert!(matches!(
            serde_json::from_slice::<Reply>(&reply.message(1, length - 1).bytes)?,
            Reply::Error(BridgeError {
                code: ErrorCode::ResourceLimit,
                ..
            })
        ));
        let oversized = match path {
            NativePath::UnixBytes(mut v) => {
                v.push(1);
                NativePath::UnixBytes(v)
            }
            NativePath::WindowsWide(mut v) => {
                v.push(1);
                NativePath::WindowsWide(v)
            }
        };
        assert!(
            super::Request {
                guard: guard(),
                action: Action::AcquireTarget {
                    catalog: None,
                    destination: oversized,
                    expected: None
                }
            }
            .validate()
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn lm_desktop_relay_configured_limits_refuse_before_authority_and_reserve_recovery()
-> anyhow::Result<()> {
    let path = NativePath::UnixBytes({
        let mut v = vec![b'x'; 12000];
        v[0] = b'/';
        v
    });
    let acquire = request(Action::AcquireTarget {
        catalog: None,
        destination: path,
        expected: None,
    });
    let length = encoded_len(&acquire, 1024 * 1024)?;
    let mut shared = super::super::tests::shared(8);
    Arc::get_mut(&mut shared).unwrap().limits.request_bytes = length - 1;
    assert!(Client::new(&shared).submit(acquire.clone()).is_err());
    assert!(shared.state.lock().unwrap().pending.is_empty());
    Arc::get_mut(&mut shared).unwrap().limits.request_bytes = length;
    let client = Client::new(&shared);
    let _authority = client.submit(acquire.clone())?;
    assert!(client.submit(acquire).is_err());
    for _ in 0..super::super::CONTROL_SLOTS {
        client.submit(request(Action::Status))?;
    }
    assert!(client.submit(request(Action::Status)).is_err());
    assert_eq!(
        shared.state.lock().unwrap().pending.len(),
        super::super::CONTROL_SLOTS + 1
    );
    shared.complete_failure();

    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let (_filesystem, mut actor) = managed_actor(&base)?;
    actor.config.limits.reply_bytes = 1;
    let destination = base.join("must-not-create");
    let denied = actor
        .migration_action(request(Action::AcquireTarget {
            catalog: None,
            destination: NativePath::from_path(&destination),
            expected: None,
        }))
        .unwrap_err();
    assert!(matches!(denied.code, ErrorCode::ResourceLimit));
    assert!(actor.migration.target.is_none());
    assert!(!destination.exists());
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_multipart_backing_is_charged_to_same_configured_pool() -> anyhow::Result<()> {
    use super::super::preview_metadata_admission::ProcessReservation;
    let config = config()?;
    let required = config.requested_preview_metadata_bytes()?;
    let mut larger = config.clone();
    larger.limits.request_bytes *= 2;
    larger.limits.reply_bytes *= 2;
    assert!(larger.requested_preview_metadata_bytes()? > required);
    let pool = crate::preview::ByteBudget::new(required)?;
    let one = pool.try_reserve(1).unwrap();
    assert!(ProcessReservation::reserve(&config, &pool).is_err());
    assert_eq!(pool.used(), 1);
    drop(one);
    let owned = ProcessReservation::reserve(&config, &pool)?;
    assert_eq!(pool.used(), required);
    drop(owned);
    assert_eq!(pool.used(), 0);
    drop(ProcessReservation::reserve(&config, &pool)?);
    assert_eq!(pool.used(), 0);
    Ok(())
}

#[test]
fn lm_desktop_relay_inspect_target_returns_pin_without_target_or_writer_authority()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_filesystem, mut actor, acquire) = open_actor(&root)?;
    let Action::AcquireTarget {
        catalog: Some(catalog),
        destination,
        expected,
    } = &acquire.action
    else {
        panic!()
    };
    let snapshot = actor.migration_action(request(Action::InspectTarget {
        catalog: catalog.clone(),
        destination: destination.clone(),
    }))?;
    assert_eq!(snapshot.phase, Phase::Inspected);
    assert_eq!(snapshot.destination, *expected);
    assert!(snapshot.sequence.is_none() && snapshot.request_digest.is_none());
    assert!(actor.migration.target.is_none());
    drop(
        actor
            .open
            .as_ref()
            .unwrap()
            .catalog
            .writers
            .enter(Priority::Foreground)?,
    );
    assert_eq!(actor.migration_action(acquire)?.phase, Phase::Target);
    drain(&mut actor);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_inspect_target_rejects_token_path_and_close_without_recovery_admission()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("catalog");
    let (_filesystem, mut actor, acquire) = open_actor(&root)?;
    let Action::AcquireTarget {
        catalog: Some(catalog),
        destination,
        ..
    } = &acquire.action
    else {
        panic!()
    };
    let inspect = request(Action::InspectTarget {
        catalog: catalog.clone(),
        destination: destination.clone(),
    });
    assert!(!inspect.action.recovery());
    assert!(
        actor
            .migration_action(request(Action::InspectTarget {
                catalog: "changed".into(),
                destination: destination.clone()
            }))
            .is_err()
    );
    assert!(
        actor
            .migration_action(request(Action::InspectTarget {
                catalog: catalog.clone(),
                destination: NativePath::from_path(root.parent().unwrap())
            }))
            .is_err()
    );
    actor.open.as_mut().unwrap().closing = true;
    assert!(matches!(
        actor.migration_action(inspect.clone()).unwrap_err().code,
        ErrorCode::Closed
    ));
    actor.open.as_mut().unwrap().closing = false;
    assert!(actor.migration.target.is_none());
    actor.close()?;
    assert!(actor.migration_action(inspect.clone()).is_err());
    let shared = super::super::tests::shared(8);
    shared.stop();
    assert!(matches!(
        Client::new(&shared).submit(inspect).err().unwrap().code,
        ErrorCode::Closed
    ));
    Ok(())
}

#[test]
#[cfg(unix)]
fn lm_desktop_relay_inspected_pin_cannot_authorize_replaced_root() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let base = temp.path().canonicalize()?;
    let root = base.join("catalog");
    let moved = base.join("original-catalog");
    let (_filesystem, mut actor, acquire) = open_actor(&root)?;
    let Action::AcquireTarget {
        catalog: Some(catalog),
        destination,
        ..
    } = &acquire.action
    else {
        panic!()
    };
    let pin = actor
        .migration_action(request(Action::InspectTarget {
            catalog: catalog.clone(),
            destination: destination.clone(),
        }))?
        .destination;
    std::fs::rename(&root, &moved)?;
    std::fs::create_dir(&root)?;
    let result = actor.migration_action(request(Action::AcquireTarget {
        catalog: Some(catalog.clone()),
        destination: destination.clone(),
        expected: pin,
    }));
    std::fs::remove_dir(&root)?;
    std::fs::rename(&moved, &root)?;
    assert!(result.is_err());
    assert!(actor.migration.target.is_none());
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_failed_acquisition_release_ack_requires_join_and_retains_target()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (_filesystem, mut actor, acquire) =
        open_actor(&temp.path().canonicalize()?.join("catalog"))?;
    actor.migration_action(acquire)?;
    let writers = actor.open.as_ref().unwrap().catalog.writers.clone();
    let hold = writers.enter(Priority::Foreground)?;
    let attempted = write(WriteKind::Bootstrap, 1, None);
    actor.migration_action(attempted.clone())?;
    actor.migration_action(request(Action::Cancel))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let released = loop {
        assert!(Instant::now() < deadline, "failed permit release deadline");
        let snapshot = actor.migration_action(release(&attempted))?;
        if snapshot.phase == Phase::Released {
            break snapshot;
        }
        assert!(matches!(
            snapshot.phase,
            Phase::Attempted | Phase::Releasing | Phase::Failed
        ));
        thread::sleep(Duration::from_millis(2));
    };
    assert!(released.failure.is_some());
    assert!(actor.migration.held());
    assert_eq!(
        actor.migration_action(release(&attempted))?.phase,
        Phase::Released
    );
    assert!(
        actor
            .migration_action(write(WriteKind::Bootstrap, 2, None))
            .is_err()
    );
    drop(hold);
    drain(&mut actor);
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_lookup_recovery_never_installs_and_preserves_exact_c_custody()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (_filesystem, mut actor, acquire) =
        open_actor(&temp.path().canonicalize()?.join("catalog"))?;
    let acquire_digest = blake3::hash(&serde_json::to_vec(&acquire)?)
        .to_hex()
        .to_string();
    let recover_target = request(Action::RecoverTarget { acquire_digest });
    assert!(matches!(
        actor.migration_request(recover_target.clone(), &Cancellation::default()),
        Reply::Refused(_)
    ));
    assert!(
        !actor.migration.held(),
        "lookup cannot acquire an absent target"
    );
    let target_reply = actor.migration_request(acquire.clone(), &Cancellation::default());
    assert!(actor.migration.held());
    assert!(matches!(
        serde_json::from_slice::<Reply>(&target_reply.message(1, 1).bytes)?,
        Reply::Error(_)
    ));
    assert_eq!(
        actor.migration_action(recover_target.clone())?.phase,
        Phase::Target
    );
    let acquire_write = write(WriteKind::Bootstrap, 1, None);
    let Action::AcquireWrite {
        sequence,
        kind,
        request_digest,
        ..
    } = &acquire_write.action
    else {
        unreachable!()
    };
    let recover_write = request(Action::RecoverWrite {
        sequence: *sequence,
        kind: *kind,
        request_digest: request_digest.clone(),
    });
    assert!(matches!(
        actor.migration_request(recover_write.clone(), &Cancellation::default()),
        Reply::Refused(_)
    ));
    assert!(
        actor.migration.target.as_ref().unwrap().attempt.is_none(),
        "lookup cannot start a missing writer"
    );
    let write_reply = actor.migration_request(acquire_write.clone(), &Cancellation::default());
    assert!(actor.migration.target.as_ref().unwrap().attempt.is_some());
    assert!(matches!(
        serde_json::from_slice::<Reply>(&write_reply.message(1, 1).bytes)?,
        Reply::Error(_)
    ));
    wait(|| actor.migration_action(recover_write.clone()).unwrap().phase == Phase::Held);
    let owner_id = actor
        .migration
        .target
        .as_ref()
        .unwrap()
        .attempt
        .as_ref()
        .unwrap()
        .owner
        .as_ref()
        .unwrap()
        .owner
        .as_ref()
        .unwrap()
        .thread()
        .id();
    actor.migration.cancel();
    for _ in 0..3 {
        assert_eq!(
            actor.migration_action(recover_target.clone())?.phase,
            Phase::Held
        );
        assert_eq!(
            actor.migration_action(recover_write.clone())?.phase,
            Phase::Held
        );
        assert_eq!(
            actor
                .migration
                .target
                .as_ref()
                .unwrap()
                .attempt
                .as_ref()
                .unwrap()
                .owner
                .as_ref()
                .unwrap()
                .owner
                .as_ref()
                .unwrap()
                .thread()
                .id(),
            owner_id
        );
    }
    assert!(matches!(
        actor.migration_request(
            request(Action::RecoverTarget {
                acquire_digest: "f".repeat(64)
            }),
            &Cancellation::default()
        ),
        Reply::Refused(_)
    ));
    assert!(matches!(
        actor.migration_request(
            request(Action::RecoverWrite {
                sequence: U64(2),
                kind: *kind,
                request_digest: request_digest.clone()
            }),
            &Cancellation::default()
        ),
        Reply::Refused(_)
    ));
    wait(|| {
        actor
            .migration_action(release(&acquire_write))
            .unwrap()
            .phase
            == Phase::Released
    });
    assert!(
        actor
            .migration
            .target
            .as_ref()
            .unwrap()
            .attempt
            .as_ref()
            .unwrap()
            .owner
            .as_ref()
            .unwrap()
            .owner
            .is_none(),
        "exact permit owner joined"
    );
    drain(&mut actor);
    assert!(!actor.migration.held());
    actor.close()?;
    Ok(())
}

#[test]
fn lm_desktop_relay_release_ack_waits_for_owner_exit_after_permit_drop() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let (_filesystem, mut actor, target) =
        open_actor(&temp.path().canonicalize()?.join("catalog"))?;
    actor.migration_action(target)?;
    let acquire = write(WriteKind::Bootstrap, 1, None);
    actor.migration_action(acquire.clone())?;
    wait(|| {
        actor
            .migration_action(request(Action::Status))
            .unwrap()
            .phase
            == Phase::Held
    });
    let target = actor.migration.target.as_ref().unwrap();
    let writers = target.writers.as_ref().unwrap().clone();
    let shared = target
        .attempt
        .as_ref()
        .unwrap()
        .owner
        .as_ref()
        .unwrap()
        .shared
        .clone();
    let (finish, gate) = mpsc::sync_channel(1);
    *shared.after_release.lock().unwrap() = Some(gate);
    assert_eq!(
        actor.migration_action(release(&acquire))?.phase,
        Phase::Releasing
    );
    wait(|| shared.state.lock().unwrap().phase == Phase::Released);
    // The real Permit is gone, but the owner is deliberately still running.
    // No externally visible Released ACK may exist before its checked join.
    drop(writers.enter(Priority::Foreground)?);
    for _ in 0..3 {
        assert_eq!(
            actor.migration_action(release(&acquire))?.phase,
            Phase::Releasing
        );
        assert!(actor.migration.held());
        assert!(
            actor
                .migration
                .target
                .as_ref()
                .unwrap()
                .attempt
                .as_ref()
                .unwrap()
                .owner
                .as_ref()
                .unwrap()
                .owner
                .as_ref()
                .is_some_and(|owner| !owner.is_finished())
        );
    }
    finish.send(())?;
    wait(|| actor.migration_action(release(&acquire)).unwrap().phase == Phase::Released);
    assert!(
        actor
            .migration
            .target
            .as_ref()
            .unwrap()
            .attempt
            .as_ref()
            .unwrap()
            .owner
            .as_ref()
            .unwrap()
            .owner
            .is_none()
    );
    drain(&mut actor);
    actor.close()?;
    Ok(())
}
