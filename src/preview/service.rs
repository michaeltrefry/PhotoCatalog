//! Application-owned preview service. All catalog/manifest mutations occur on
//! the owner; native workers can only return isolated, validated image results.
use super::*;
use crate::{Catalog, catalog_metadata::RenderIdentity, storage_volume::NativePath};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierPolicy {
    pub edge: u32,
    pub encoding: CodecSettings,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewPolicy {
    pub thumbnail: TierPolicy,
    pub large: TierPolicy,
}
impl Default for PreviewPolicy {
    fn default() -> Self {
        let encoding = CodecSettings {
            codec: Codec::Jpeg,
            quality: 80,
        };
        Self {
            thumbnail: TierPolicy {
                edge: 512,
                encoding,
            },
            large: TierPolicy {
                edge: 1600,
                encoding,
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceLimits {
    pub requests: usize,
    pub workers: usize,
    /// Admission reservations for full renderer, native/codec scratch and its
    /// temporary encoded Vec. Measured RSS is separate evidence, not this count.
    pub working_bytes: u64,
    pub per_worker_bytes: u64,
    pub decode_limits: crate::media::DecodeLimits,
    pub encoded_staging_bytes: u64,
    pub per_worker_encoded_bytes: u64,
    pub decoded_cache_bytes: u64,
    pub decoded_live_bytes: u64,
    pub decoded_entries: usize,
}
impl Default for ServiceLimits {
    fn default() -> Self {
        Self {
            decode_limits: crate::media::DecodeLimits {
                max_encoded_bytes: 256 * 1024 * 1024,
                max_intermediate_pixels: 32_000_000,
                max_allocation_bytes: 768 * 1024 * 1024,
            },
            requests: 400,
            workers: 1,
            working_bytes: 3 * 1024 * 1024 * 1024,
            per_worker_bytes: 3 * 1024 * 1024 * 1024,
            encoded_staging_bytes: 32 * 1024 * 1024,
            per_worker_encoded_bytes: 8 * 1024 * 1024,
            decoded_cache_bytes: 256 * 1024 * 1024,
            decoded_live_bytes: 256 * 1024 * 1024,
            decoded_entries: 400,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedJob {
    request: RenderWork,
    expected: RenderIdentity,
    import: bool,
    state: JobState,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    NeedsResources(String),
    Unavailable(String),
    Failed(String),
}
#[derive(Debug, Serialize)]
pub struct JobView {
    pub cursor: i64,
    pub id: String,
    pub asset: String,
    pub state: JobState,
}

struct ActiveJob {
    lease: WorkLease,
    worker: WorkerProcess,
    _encoded: ByteReservation,
}
#[derive(Debug, Clone, Serialize)]
pub enum ServiceCompletion {
    Ready,
    Stale,
    NeedsResources(String),
    Unavailable(String),
    Failed(String),
    Canceled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceEvent {
    ManifestAttached,
    BeforeCatalogCommit,
    CatalogCommitted,
    JournalRemoving,
}
pub struct EncodedPreview {
    bytes: Vec<u8>,
    _reservation: ByteReservation,
}
impl EncodedPreview {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
pub struct PreviewView {
    pub key: Option<PreviewKey>,
    pub legacy_hash: Option<String>,
    pub pixels: Arc<RetainedPixels>,
    pub record: Option<RenderRecord>,
    pub stale: bool,
}
type ServiceObserver = Box<dyn FnMut(ServiceEvent) -> Result<()> + Send>;
pub struct PreviewService {
    observer: std::cell::RefCell<Option<ServiceObserver>>,
    store: PreviewStore,
    decoded: DecodedCache,
    scheduler: PreviewScheduler,
    executable: PathBuf,
    staging: PathBuf,
    limits: ServiceLimits,
    policy: PreviewPolicy,
    encoded: ByteBudget,
    jobs: HashMap<String, SavedJob>,
    active: HashMap<u64, ActiveJob>,
    consumers: HashMap<Consumer, String>,
    completed: HashMap<Consumer, ServiceCompletion>,
}
fn same_pixels(a: &RenderIdentity, b: &RenderIdentity) -> bool {
    a.asset_id == b.asset_id
        && a.generation == b.generation
        && a.fingerprint == b.fingerprint
        && a.state == b.state
}
impl PreviewService {
    pub fn open(
        config: StoreConfig,
        original_roots: &[PathBuf],
        executable: PathBuf,
        policy: PreviewPolicy,
        limits: ServiceLimits,
    ) -> Result<Self> {
        limits.decode_limits.validate()?;
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "preview worker executable unavailable"
        );
        ensure!(
            limits.per_worker_bytes > 0 && limits.per_worker_bytes <= limits.working_bytes,
            "worker working admission"
        );
        ensure!(
            limits.per_worker_encoded_bytes > 0
                && limits.per_worker_encoded_bytes <= limits.encoded_staging_bytes / 2,
            "reserve encoded staging headroom for foreground reads"
        );
        for tier in [&policy.thumbnail, &policy.large] {
            ensure!((1..=8192).contains(&tier.edge), "preview policy dimensions");
            tier.encoding.validate()?;
        }
        let config = PreviewStore::current_configuration(config)?;
        let store = PreviewStore::open(config, original_roots)?;
        let staging = store.configuration().manifest_root.join("workers");
        recover_worker_staging(&staging, 128)?;
        Ok(Self {
            observer: std::cell::RefCell::new(None),
            store,
            decoded: DecodedCache::new(
                limits.decoded_cache_bytes,
                limits.decoded_live_bytes,
                limits.decoded_entries,
            )?,
            scheduler: PreviewScheduler::new(SchedulerLimits {
                requests: limits.requests,
                workers: limits.workers,
                working_bytes: limits.working_bytes,
            })?,
            executable,
            staging,
            encoded: ByteBudget::new(limits.encoded_staging_bytes)?,
            limits,
            policy,
            jobs: HashMap::new(),
            active: HashMap::new(),
            consumers: HashMap::new(),
            completed: HashMap::new(),
        })
    }
    /// Optional short diagnostics callback. Recovery probes can stop an actual
    /// owner process at named durable boundaries without changing normal ordering.
    pub fn set_observer(
        &mut self,
        observer: impl FnMut(ServiceEvent) -> Result<()> + Send + 'static,
    ) {
        self.observer.replace(Some(Box::new(observer)));
    }
    fn observe(&self, event: ServiceEvent) -> Result<()> {
        if let Some(observer) = self.observer.borrow_mut().as_mut() {
            observer(event)?;
        }
        Ok(())
    }
    pub fn key(
        &self,
        identity: &RenderIdentity,
        tier: Tier,
        fingerprint: &str,
    ) -> Result<PreviewKey> {
        let policy = match tier {
            Tier::Thumbnail => &self.policy.thumbnail,
            Tier::Large => &self.policy.large,
        };
        let key = PreviewKey {
            asset_id: identity.asset_id.clone(),
            variant_id: "master".into(),
            generation: u64::try_from(identity.generation)?,
            fingerprint: fingerprint.into(),
            edit_revision: 0,
            renderer_version: renderer_identity().into(),
            preparation_version: PREPARATION_VERSION.into(),
            tier,
            edge: policy.edge,
            encoding: policy.encoding,
        };
        key.validate()?;
        Ok(key)
    }
    pub fn cached(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        tier: Tier,
        allow_stale: bool,
    ) -> Result<Option<PreviewView>> {
        let identity = catalog.render_identity(asset)?;
        let key = self.key(
            &identity,
            tier,
            identity.fingerprint.as_deref().unwrap_or(&"0".repeat(64)),
        )?;
        let allowance = self.limits.encoded_staging_bytes - self.encoded.used();
        let _reservation = self
            .encoded
            .try_reserve(allowance)
            .context("encoded staging unavailable")?;
        let Some(cached) = self.store.read_limited(&key, allow_stale, allowance)? else {
            if allow_stale
                && tier == Tier::Thumbnail
                && let Some((hash, bytes)) = catalog.retained_legacy_preview(asset, allowance)?
            {
                let (width, height) = encoded_dimensions(&bytes, Codec::Jpeg)?;
                let pixels = self.decoded.decode(
                    blake3::hash(format!("legacy:{hash}").as_bytes())
                        .to_hex()
                        .to_string(),
                    &bytes,
                    Codec::Jpeg,
                    width,
                    height,
                )?;
                return Ok(Some(PreviewView {
                    key: None,
                    legacy_hash: Some(hash),
                    pixels,
                    record: None,
                    stale: true,
                }));
            }
            return Ok(None);
        };
        let dimensions = encoded_dimensions(&cached.bytes, cached.key.encoding.codec);
        let pixels = (|| -> Result<_> {
            let (width, height) = dimensions?;
            ensure!(
                width <= cached.key.edge && height <= cached.key.edge,
                "cached image exceeds tier"
            );
            if let Some(record) = &cached.record {
                ensure!(
                    (record.width, record.height) == (width, height),
                    "cached render record dimensions mismatch"
                );
            }
            self.decoded.decode(
                cached.key.digest()?,
                &cached.bytes,
                cached.key.encoding.codec,
                width,
                height,
            )
        })();
        let pixels = match pixels {
            Ok(pixels) => pixels,
            Err(error) => {
                if error.downcast_ref::<DecodedBudgetExceeded>().is_none() {
                    self.store.invalidate(&cached.key)?;
                }
                return Err(error);
            }
        };
        let current = catalog.render_identity(asset)?;
        let stale = cached.stale || !same_pixels(&identity, &current) || current.state != "ready";
        if stale && !(allow_stale && tier == Tier::Thumbnail) {
            return Ok(None);
        }
        Ok(Some(PreviewView {
            key: Some(cached.key),
            legacy_hash: None,
            record: cached.record,
            pixels,
            stale,
        }))
    }
    /// Validated original cache payload with an encoded-memory reservation that
    /// remains live until the caller finishes exporting/drops the result.
    pub fn encoded_cached(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        tier: Tier,
        allow_stale: bool,
    ) -> Result<Option<EncodedPreview>> {
        let Some(view) = self.cached(catalog, asset, tier, allow_stale)? else {
            return Ok(None);
        };
        let allowance = self.limits.encoded_staging_bytes - self.encoded.used();
        let reservation = self
            .encoded
            .try_reserve(allowance)
            .context("encoded export admission")?;
        let bytes = if let Some(key) = view.key {
            self.store
                .read_limited(&key, false, allowance)?
                .context("cache changed during export")?
                .bytes
        } else {
            catalog
                .retained_legacy_preview(asset, allowance)?
                .context("legacy preview changed during export")?
                .1
        };
        Ok(Some(EncodedPreview {
            bytes,
            _reservation: reservation,
        }))
    }
    /// Explicit sources are supplied by the import walker after durable reserve
    /// and metadata capture. Browse misses resolve the current tagged catalog path.
    pub fn submit_import(
        &mut self,
        catalog: &mut Catalog,
        asset: &str,
        source: &Path,
        fingerprint: &str,
    ) -> Result<Consumer> {
        let expected = catalog.render_identity(asset)?;
        ensure!(
            expected.state == "pending",
            "import preview requires reserved catalog asset"
        );
        let key = self.key(&expected, Tier::Thumbnail, fingerprint)?;
        self.submit(
            catalog,
            SavedJob {
                request: RenderWork {
                    source: NativePath::from_path(source),
                    keys: vec![key],
                    encoded_limit: self.limits.per_worker_encoded_bytes,
                    decode_limits: self.limits.decode_limits,
                },
                expected,
                import: true,
                state: JobState::Queued,
            },
            Priority::Background,
        )
    }
    pub fn request(
        &mut self,
        catalog: &mut Catalog,
        asset: &str,
        tier: Tier,
        priority: Priority,
    ) -> Result<Consumer> {
        let expected = catalog.render_identity(asset)?;
        ensure!(
            expected.state == "ready",
            "original is awaiting import completion"
        );
        let fingerprint = expected
            .fingerprint
            .as_deref()
            .context("original revision unavailable")?;
        let key = self.key(&expected, tier, fingerprint)?;
        let source = catalog.preview_original_path(asset)?;
        self.submit(
            catalog,
            SavedJob {
                request: RenderWork {
                    source,
                    keys: vec![key],
                    encoded_limit: self.limits.per_worker_encoded_bytes,
                    decode_limits: self.limits.decode_limits,
                },
                expected,
                import: false,
                state: JobState::Queued,
            },
            priority,
        )
    }
    fn submit(
        &mut self,
        catalog: &mut Catalog,
        job: SavedJob,
        priority: Priority,
    ) -> Result<Consumer> {
        ensure!(
            self.consumers.len() < self.limits.requests,
            "preview consumer limit"
        );
        let id = blake3::hash(&serde_json::to_vec(&job.request.keys)?)
            .to_hex()
            .to_string();
        let stored = serde_json::to_string(&job)?;
        let admitted = catalog.with_render_identity(&job.expected, || {
            self.store.save_job(&id, &stored, self.limits.requests)?;
            for key in &job.request.keys {
                self.store.desire(key, || Ok(true))?;
            }
            Ok(())
        })?;
        ensure!(admitted.is_some(), "stale preview request");
        let consumer =
            self.scheduler
                .request(id.clone(), self.limits.per_worker_bytes, priority)?;
        self.consumers.insert(consumer, id.clone());
        self.jobs.insert(id, job);
        Ok(consumer)
    }
    pub fn cancel(&mut self, consumer: Consumer) -> Result<()> {
        self.scheduler.cancel(consumer);
        self.completed.remove(&consumer);
        if let Some(id) = self.consumers.remove(&consumer)
            && !self.consumers.values().any(|key| key == &id)
            && !self.active.values().any(|active| active.lease.key == id)
            && let Some(job) = self.jobs.remove(&id)
            && !job.import
        {
            self.store.finish_job(&id)?;
        }
        Ok(())
    }
    /// One bounded owner iteration. Native work remains in at most `workers`
    /// children; stale/canceled children are joined before reservations release.
    pub fn tick(&mut self, catalog: &mut Catalog) -> Result<()> {
        let active_ids = self.active.keys().copied().collect::<Vec<_>>();
        for lease_id in active_ids {
            let result = {
                let active = self.active.get_mut(&lease_id).unwrap();
                active.worker.poll(&active.lease.canceled)
            };
            if matches!(result, Ok(None)) {
                continue;
            }
            let active = self.active.remove(&lease_id).unwrap();
            let id = active.lease.key.clone();
            let canceled = active
                .lease
                .canceled
                .load(std::sync::atomic::Ordering::Acquire);
            let job = self
                .jobs
                .get(&id)
                .context("active preview job missing")?
                .clone();
            let publication = if canceled {
                Ok(ServiceCompletion::Canceled)
            } else {
                result.and_then(|batch| {
                    self.publish_batch(catalog, &job, batch.context("missing worker result")?)
                })
            };
            // Drop confirms process exit and releases encoded staging only after
            // complete validation/publication has consumed the returned buffers.
            drop(active);
            let outcome = if canceled {
                WorkerOutcome::Stopped
            } else if publication.is_ok() {
                WorkerOutcome::Succeeded
            } else {
                WorkerOutcome::Failed
            };
            let completion = self.scheduler.finished(lease_id, outcome)?;
            if completion.requeued || completion.superseded {
                continue;
            }
            let status = match publication {
                Ok(status) => status,
                Err(error) => self.record_failure(&id, &job, error)?,
            };
            if matches!(status, ServiceCompletion::Ready | ServiceCompletion::Stale)
                || (matches!(status, ServiceCompletion::Canceled) && !job.import)
            {
                self.observe(ServiceEvent::JournalRemoving)?;
                self.store.finish_job(&id)?;
            }
            self.jobs.remove(&id);
            for consumer in completion.consumers {
                self.completed.insert(consumer, status.clone());
            }
        }
        if self.store.relocation_pending()? {
            return Ok(());
        }
        while self.encoded.used() + self.limits.per_worker_encoded_bytes
            <= self.limits.encoded_staging_bytes / 2
        {
            let Some(lease) = self.scheduler.next_ready()? else {
                break;
            };
            let id = lease.key.clone();
            let job = self
                .jobs
                .get(&id)
                .context("queued preview job missing")?
                .clone();
            let current = catalog.render_identity(&job.expected.asset_id)?;
            if !same_pixels(&current, &job.expected) {
                let completion = self.scheduler.finished(lease.id, WorkerOutcome::Failed)?;
                self.store.finish_job(&id)?;
                self.jobs.remove(&id);
                for consumer in completion.consumers {
                    self.completed.insert(consumer, ServiceCompletion::Stale);
                }
                continue;
            }
            let guard = self
                .encoded
                .try_reserve(self.limits.per_worker_encoded_bytes)
                .context("worker staging admission")?;
            match WorkerProcess::spawn(&self.executable, &self.staging, job.request.clone()) {
                Ok(worker) => {
                    self.active.insert(
                        lease.id,
                        ActiveJob {
                            lease,
                            worker,
                            _encoded: guard,
                        },
                    );
                }
                Err(error) => {
                    drop(guard);
                    let completion = self.scheduler.finished(lease.id, WorkerOutcome::Failed)?;
                    let status = self.record_failure(&id, &job, error)?;
                    self.jobs.remove(&id);
                    for consumer in completion.consumers {
                        self.completed.insert(consumer, status.clone());
                    }
                }
            }
        }
        Ok(())
    }
    fn publish_batch(
        &self,
        catalog: &mut Catalog,
        job: &SavedJob,
        batch: RenderedPreviewBatch,
    ) -> Result<ServiceCompletion> {
        ensure!(
            !job.import || batch.objects.len() == 1,
            "import publication requires one retained tier"
        );
        let mut stale = false;
        for object in &batch.objects {
            let record = RenderRecord {
                width: object.pixels.width(),
                height: object.pixels.height(),
                metadata: batch.metadata.clone(),
                provenance: batch.provenance.clone(),
            };
            // File staging and checksum happen before the catalog writer guard.
            // Only the final manifest attachment and catalog ready transition
            // occur while the catalog generation is held authoritative.
            let publication =
                self.store
                    .publish_record(&object.key, &object.encoded, &record, |attach| {
                        let result = if job.import {
                            catalog.commit_preview_import(
                                &job.expected,
                                &object.key.fingerprint,
                                &batch.metadata,
                                &object.key.digest()?,
                                || {
                                    let publication = attach()?;
                                    self.observe(ServiceEvent::ManifestAttached)?;
                                    Ok(publication)
                                },
                                || self.observe(ServiceEvent::BeforeCatalogCommit),
                            )?
                        } else {
                            catalog.with_render_identity(&job.expected, attach)?
                        };
                        if job.import && result.is_some() {
                            self.observe(ServiceEvent::CatalogCommitted)?;
                        }
                        Ok(result.unwrap_or(Publication::Stale))
                    })?;
            stale |= publication == Publication::Stale;
        }
        Ok(if stale {
            ServiceCompletion::Stale
        } else {
            ServiceCompletion::Ready
        })
    }
    fn record_failure(
        &self,
        id: &str,
        job: &SavedJob,
        error: anyhow::Error,
    ) -> Result<ServiceCompletion> {
        let message = format!("{error:#}");
        let message = message.chars().take(4096).collect::<String>();
        let kind = error
            .downcast_ref::<WorkerFailure>()
            .and_then(|failure| failure.decode_status);
        let resources = kind == Some(crate::media::DecodeStatus::ResourceLimit)
            || error.downcast_ref::<CacheQuotaExceeded>().is_some()
            || error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
            });
        let unavailable = kind == Some(crate::media::DecodeStatus::Io)
            || error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    )
                })
            });
        let (state, status) = if resources {
            (
                JobState::NeedsResources(message.clone()),
                ServiceCompletion::NeedsResources(message),
            )
        } else if unavailable {
            (
                JobState::Unavailable(message.clone()),
                ServiceCompletion::Unavailable(message),
            )
        } else {
            (
                JobState::Failed(message.clone()),
                ServiceCompletion::Failed(message),
            )
        };
        let mut saved = job.clone();
        saved.state = state;
        self.store
            .save_job(id, &serde_json::to_string(&saved)?, self.limits.requests)?;
        Ok(status)
    }
    pub fn jobs(&self, after: i64, limit: usize) -> Result<Vec<JobView>> {
        self.store
            .saved_jobs(after, limit)?
            .into_iter()
            .map(|(cursor, id, descriptor)| {
                let job: SavedJob = serde_json::from_str(&descriptor)?;
                Ok(JobView {
                    cursor,
                    id,
                    asset: job.expected.asset_id,
                    state: job.state,
                })
            })
            .collect()
    }
    /// Bounded, explicit restart/retry. Returned consumer handles must be drained
    /// just like newly submitted requests. Resource/IO failures are not auto-spun.
    pub fn resume(
        &mut self,
        catalog: &mut Catalog,
        after: i64,
        limit: usize,
        retry_blocked: bool,
    ) -> Result<(i64, Vec<Consumer>)> {
        let mut cursor = after;
        let mut consumers = Vec::new();
        let result = (|| -> Result<()> {
            for (position, id, descriptor) in self.store.saved_jobs(after, limit)? {
                if self.consumers.len() >= self.limits.requests {
                    break;
                }
                cursor = position;
                if self.jobs.contains_key(&id) {
                    continue;
                }
                let mut job: SavedJob = serde_json::from_str(&descriptor)?;
                let current = catalog.render_identity(&job.expected.asset_id)?;
                let key = &job.request.keys[0];
                // Crash after catalog commit but before journal deletion.
                if job.import
                    && current.state == "ready"
                    && current.generation == job.expected.generation
                    && current.fingerprint.as_deref() == Some(&key.fingerprint)
                {
                    self.store.finish_job(&id)?;
                    continue;
                }
                if !same_pixels(&current, &job.expected) {
                    self.store.finish_job(&id)?;
                    continue;
                }
                // Crash after manifest attachment but before catalog ready commit.
                if job.import
                    && self.store.current_is_intact(key)?
                    && let Some(record) = self.store.render_record(key)?
                    && catalog
                        .commit_preview_import(
                            &job.expected,
                            &key.fingerprint,
                            &record.metadata,
                            &key.digest()?,
                            || Ok(()),
                            || Ok(()),
                        )?
                        .is_some()
                {
                    self.store.finish_job(&id)?;
                    continue;
                }
                if !retry_blocked && !matches!(job.state, JobState::Queued) {
                    continue;
                }
                let fingerprint = job.request.keys[0].fingerprint.clone();
                job.request.keys = job
                    .request
                    .keys
                    .iter()
                    .map(|key| self.key(&current, key.tier, &fingerprint))
                    .collect::<Result<Vec<_>>>()?;
                job.request.source = catalog.preview_original_path(&job.expected.asset_id)?;
                job.request.decode_limits = self.limits.decode_limits;
                job.request.encoded_limit = self.limits.per_worker_encoded_bytes;
                job.state = JobState::Queued;
                let new_id = blake3::hash(&serde_json::to_vec(&job.request.keys)?)
                    .to_hex()
                    .to_string();
                consumers.push(self.submit(catalog, job, Priority::Background)?);
                if new_id != id {
                    self.store.finish_job(&id)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            // resume does not launch children. Roll back only this call's
            // in-memory admissions while preserving every durable retry job.
            // The caller never loses an inaccessible live consumer on Err.
            for consumer in consumers {
                self.scheduler.cancel(consumer);
                if let Some(id) = self.consumers.remove(&consumer)
                    && !self.consumers.values().any(|other| other == &id)
                    && !self.active.values().any(|active| active.lease.key == id)
                {
                    self.jobs.remove(&id);
                }
            }
            return Err(error);
        }
        Ok((cursor, consumers))
    }
    /// Synchronous import owns all handles it creates. Applications sharing the
    /// service with foreground work use ImportSession::advance instead.
    pub fn is_drained(&self) -> bool {
        self.consumers.is_empty() && self.active.is_empty() && self.scheduler.usage().queued == 0
    }
    pub fn take_completion(&mut self, consumer: Consumer) -> Option<ServiceCompletion> {
        let result = self.completed.remove(&consumer);
        if result.is_some() {
            self.consumers.remove(&consumer);
        }
        result
    }
    pub fn set_cache_budgets(&mut self, thumbnail: u64, large: u64) -> Result<()> {
        self.store.set_budgets(thumbnail, large)
    }
    pub fn cache_configuration(&self) -> &StoreConfig {
        self.store.configuration()
    }
    pub fn begin_relocation(
        &mut self,
        tier: Tier,
        destination: &Path,
        original_roots: &[PathBuf],
    ) -> Result<()> {
        let usage = self.scheduler.usage();
        ensure!(
            usage.active == 0 && usage.queued == 0,
            "drain or cancel preview work before relocation"
        );
        self.store
            .begin_relocation(tier, destination, original_roots)
    }
    pub fn relocation_step(
        &mut self,
        tier: Tier,
        limit: usize,
        bytes: u64,
    ) -> Result<RelocationProgress> {
        self.store.relocation_step(tier, limit, bytes)
    }
    pub fn available_request_slots(&self) -> usize {
        self.limits.requests.saturating_sub(self.consumers.len())
    }
    pub fn scheduler_usage(&self) -> SchedulerUsage {
        self.scheduler.usage()
    }
    pub fn store_usage(&self) -> Result<StoreUsage> {
        self.store.usage()
    }
    pub fn decoded_live_bytes(&self) -> u64 {
        self.decoded.live_bytes()
    }
    pub fn clear_decoded_cache(&mut self) {
        self.decoded.clear();
    }
    pub fn maintenance(&self) -> Result<()> {
        self.store.flush_touches()?;
        self.store.recover(128)?;
        recover_worker_staging(&self.staging, 128)?;
        Ok(())
    }
}

impl Drop for PreviewService {
    fn drop(&mut self) {
        // Join workers before releasing the manifest owner's process lock.
        self.active.clear();
    }
}
