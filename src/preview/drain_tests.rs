//! Real owned child, injected wait errors; no image decoder or GPU work.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn fixture_child() {
    if std::env::var_os("PHOTOCATALOG_PREVIEW_DRAIN_FIXTURE").is_some() {
        use std::io::Read;
        // Only the parent owns the write end. EOF also bounds cleanup if the
        // parent test aborts before reaching its normal explicit stop.
        let _ = std::io::stdin().read(&mut [0]);
    }
}
impl PreviewService {
    pub(crate) fn test_owned_worker(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        source: &Path,
        failures: Arc<AtomicUsize>,
    ) -> (u32, Consumer) {
        let job = super::recovery_tests::import_job(catalog, self, asset, source);
        let id = blake3::hash(&serde_json::to_vec(&job.request.keys).unwrap())
            .to_hex()
            .to_string();
        self.store
            .save_job(&id, &serde_json::to_string(&job).unwrap(), 400)
            .unwrap();
        let consumer = self
            .scheduler
            .request(id.clone(), 4096, Priority::Background)
            .unwrap();
        let lease = self.scheduler.next_ready().unwrap().unwrap();
        let encoded = self.encoded.try_reserve(2048).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "preview::service::drain_tests::fixture_child",
                "--nocapture",
            ])
            .env("PHOTOCATALOG_PREVIEW_DRAIN_FIXTURE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let worker = WorkerProcess::test_child(
            child,
            self.store
                .configuration()
                .manifest_root
                .join("drain-fixture-staging"),
            job.request.clone(),
            failures,
        );
        self.jobs.insert(id, job);
        self.active.insert(
            lease.id,
            ActiveJob {
                lease,
                worker,
                _encoded: encoded,
            },
        );
        (pid, consumer)
    }
}

#[test]
fn failed_drain_retains_worker_budgets_cache_and_can_retry() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, mut service, asset, source) = super::recovery_tests::setup(root.path());
    let config = service.cache_configuration().clone();
    let failures = Arc::new(AtomicUsize::new(1));
    let (pid, _) = service.test_owned_worker(&catalog, &asset, &source, failures);
    let usage = service.scheduler_usage();
    assert_eq!(usage.reserved_bytes, 4096);
    assert!(service.try_shutdown().is_err());
    // A failed wait is unverified even if kill succeeded. Retain the actual
    // Child owner and reservations until a later successful wait proves exit.
    assert_eq!(service.active_worker_pids(), vec![pid]);
    assert_eq!(service.scheduler_usage(), usage);
    assert_eq!(service.encoded.used(), 2048);
    assert!(PreviewStore::open(config.clone(), &[]).is_err());
    assert!(service.pause_native_launches().is_err());
    assert!(service.tick(&mut catalog).is_err());
    assert!(
        service
            .request(&mut catalog, &asset, Tier::Thumbnail, Priority::Foreground)
            .is_err()
    );
    service.try_shutdown().unwrap();
    assert!(service.native_work_drained());
    assert_eq!(service.scheduler_usage().reserved_bytes, 0);
    assert_eq!(service.encoded.used(), 0);
    assert!(
        PreviewStore::open(config.clone(), &[]).is_err(),
        "cache belongs to live service until dropped"
    );
    drop(service);
    drop(PreviewStore::open(config, &[]).unwrap());
}

#[test]
fn poll_error_does_not_complete_or_release_before_reap() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, mut service, asset, source) = super::recovery_tests::setup(root.path());
    let failures = Arc::new(AtomicUsize::new(2));
    let (pid, consumer) = service.test_owned_worker(&catalog, &asset, &source, failures);
    // Preserve the consumer while requesting stop, as foreground preemption does.
    service
        .active
        .values()
        .next()
        .unwrap()
        .lease
        .canceled
        .store(true, Ordering::Release);
    assert!(service.tick(&mut catalog).is_err());
    assert_eq!(service.active_worker_pids(), vec![pid]);
    assert_eq!(service.scheduler_usage().reserved_bytes, 4096);
    assert_eq!(service.encoded.used(), 2048);
    assert!(service.take_completion(consumer).is_none());
    service.tick(&mut catalog).unwrap();
    assert!(service.native_work_drained());
    assert_eq!(service.scheduler_usage().reserved_bytes, 0);
    assert_eq!(service.encoded.used(), 0);
    assert!(matches!(
        service.take_completion(consumer),
        Some(ServiceCompletion::Canceled)
    ));
}

#[test]
fn initial_send_error_retains_registered_owner_until_reap() {
    let root = tempfile::tempdir().unwrap();
    let (mut catalog, mut service, asset, source) = super::recovery_tests::setup(root.path());
    let failures = Arc::new(AtomicUsize::new(1));
    let (pid, consumer) = service.test_owned_worker(&catalog, &asset, &source, failures);
    service
        .active
        .values_mut()
        .next()
        .unwrap()
        .worker
        .test_initial_send_failure();
    // Service dispatch records errors before its next poll; repeated start must
    // retain that error rather than resend partial input or forget the failure.
    assert!(
        service
            .active
            .values_mut()
            .next()
            .unwrap()
            .worker
            .start()
            .is_err()
    );
    assert!(
        service
            .active
            .values_mut()
            .next()
            .unwrap()
            .worker
            .start()
            .is_err()
    );
    assert!(service.tick(&mut catalog).is_err());
    assert_eq!(service.active_worker_pids(), vec![pid]);
    assert_eq!(service.scheduler_usage().reserved_bytes, 4096);
    assert_eq!(service.encoded.used(), 2048);
    assert!(service.take_completion(consumer).is_none());
    service.try_shutdown().unwrap();
    assert!(service.native_work_drained());
    assert_eq!(service.scheduler_usage().reserved_bytes, 0);
    assert_eq!(service.encoded.used(), 0);
}
