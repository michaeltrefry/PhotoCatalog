//! Application-owned preview service. All catalog/manifest mutations occur on
//! the owner; native workers can only return isolated, validated image results.
#[path = "read_queue.rs"]
mod read_queue;
use super::*;
use crate::{
    Catalog,
    catalog_edits::{EditRenderIdentity, MASTER, VariantKey},
    catalog_metadata::RenderIdentity,
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
pub use read_queue::{ReadCompletion, ReadOutcome, ReadQueueUsage, ReadTicket};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
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
    #[serde(default = "default_prepared_bytes")]
    pub prepared_cache_bytes: u64,
    #[serde(default = "default_prepared_entries")]
    pub prepared_cache_entries: usize,
}
fn default_prepared_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_prepared_entries() -> usize {
    16
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
            per_worker_bytes: 2164 * 1024 * 1024,
            encoded_staging_bytes: 32 * 1024 * 1024,
            per_worker_encoded_bytes: 8 * 1024 * 1024,
            decoded_cache_bytes: 256 * 1024 * 1024,
            decoded_live_bytes: 256 * 1024 * 1024,
            decoded_entries: 400,
            prepared_cache_bytes: default_prepared_bytes(),
            prepared_cache_entries: default_prepared_entries(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedJob {
    request: RenderWork,
    expected: RenderIdentity,
    #[serde(default)]
    edit: Option<EditRenderIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import_image: Option<EditRenderIdentity>,
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
#[derive(Debug, Default, Clone, Serialize)]
pub struct CacheReadMetrics {
    pub catalog_identity_ms: f64,
    /// Includes manifest SQL, record parsing, filesystem read and checksum.
    pub store_read_checksum_ms: f64,
    /// Header parsing plus full RGB8 decode, or an existing decoded-cache hit.
    pub header_decode_ms: f64,
    pub total_ms: f64,
    pub decoded_hits: u64,
    pub decoded_misses: u64,
    pub returned_pixels: bool,
}
enum ReadPhase {
    Identity,
    Store,
    Decode,
}
fn measured<T>(
    metrics: &mut Option<&mut CacheReadMetrics>,
    phase: ReadPhase,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let start = metrics.as_ref().map(|_| Instant::now());
    let result = operation();
    if let (Some(metrics), Some(start)) = (metrics.as_deref_mut(), start) {
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        match phase {
            ReadPhase::Identity => metrics.catalog_identity_ms += elapsed,
            ReadPhase::Store => metrics.store_read_checksum_ms += elapsed,
            ReadPhase::Decode => metrics.header_decode_ms += elapsed,
        }
    }
    result
}
type ServiceObserver = Box<dyn FnMut(ServiceEvent) -> Result<()> + Send>;
/// Holds native launch admission while another owned worker (for example an
/// export) runs. The owner must join that worker before dropping the token.
/// Existing workers are not stopped automatically: cancel/drain them first.
pub struct NativeLaunchPause(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for NativeLaunchPause {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
/// Diagnostic receipt for the most recently consumed successful worker result.
/// Keys identify the producer; a later cache hit is not a new worker measurement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResourceMetrics {
    pub pid: u32,
    pub keys: Vec<PreviewKey>,
    pub peak_resident_bytes: Option<u64>,
    pub peak_method: String,
}
pub struct PreviewService {
    launch_pauses: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reads: read_queue::ReadQueue,
    prepared: super::prepared_cache::PreparedCache,
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
    worker_metrics: Option<WorkerResourceMetrics>,
}
fn same_pixels(a: &RenderIdentity, b: &RenderIdentity) -> bool {
    a.asset_id == b.asset_id
        && a.generation == b.generation
        && a.fingerprint == b.fingerprint
        && a.state == b.state
}
fn same_edit(a: &EditRenderIdentity, b: &EditRenderIdentity) -> bool {
    same_pixels(&a.source, &b.source)
        && match (&a.image_identity, &b.image_identity) {
            (Some(a), Some(b)) => {
                a.image_id == b.image_id
                    && a.key == b.key
                    && a.pixel_generation == b.pixel_generation
                    && a.shared_source_epoch == b.shared_source_epoch
                    && a.physical_generation == b.physical_generation
            }
            (None, None) => true,
            _ => false,
        }
        && a.key == b.key
        && a.revision == b.revision
        && a.recipe_digest == b.recipe_digest
}
// Preview pixels ignore rating/label-only revisions. Publication still acquires
// the existing exact current identity CAS, so export authority is unchanged.
fn current_edit(catalog: &Catalog, expected: &EditRenderIdentity) -> Result<EditRenderIdentity> {
    let mut current = catalog.edit_render_identity(&expected.key)?;
    if expected.image_identity.is_none() {
        current.source = catalog.render_identity(&expected.key.asset_id)?;
        current.image_identity = None;
    }
    Ok(current)
}
fn with_preview_transaction<T>(
    catalog: &mut Catalog,
    expected: &EditRenderIdentity,
    priority: crate::catalog_writer::Priority,
    attach: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let current = current_edit(catalog, expected)?;
    if !same_edit(expected, &current) {
        return Ok(None);
    }
    catalog.with_edit_transaction(&current, priority, attach)
}
fn import_image_current(
    tx: &rusqlite::Transaction<'_>,
    expected: Option<&EditRenderIdentity>,
    published: bool,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    let image = expected
        .image_identity
        .as_ref()
        .context("missing scoped import identity")?;
    let current = crate::catalog_images::identity(tx, &image.image_id)?;
    let revision: i64 = tx.query_row(
        "SELECT COALESCE((SELECT revision FROM edit_variants WHERE asset_id=?1 AND id='master'),0)",
        [&expected.key.asset_id],
        |r| r.get(0),
    )?;
    Ok(current.image_id == image.image_id
        && current.key == image.key
        && current.pixel_generation == image.pixel_generation
        && current.shared_source_epoch == image.shared_source_epoch
        && current.physical_generation
            == image
                .physical_generation
                .checked_add(i64::from(published))
                .context("physical generation overflow")?
        && revision == 0)
}
impl SavedJob {
    fn validate(&self) -> Result<()> {
        self.request.validate_persisted()?;
        if let Some(image) = &self.import_image {
            ensure!(
                self.import
                    && self.edit.is_none()
                    && image
                        .image_identity
                        .as_ref()
                        .is_some_and(|identity| identity.key == image.key
                            && identity.physical_generation == image.source.generation)
                    && image.key == VariantKey::master(&self.expected.asset_id)
                    && image.revision == 0
                    && image.source.asset_id == self.expected.asset_id
                    && image.source.state == self.expected.state
                    && image.source.fingerprint == self.expected.fingerprint,
                "mixed scoped import authority"
            );
        }
        let key_source = self
            .import_image
            .as_ref()
            .map(|image| &image.source)
            .unwrap_or(&self.expected);
        ensure!(
            self.request
                .keys
                .iter()
                .all(|key| key.asset_id == self.expected.asset_id
                    && Some(key.generation)
                        == u64::try_from(key_source.generation)
                            .ok()
                            .and_then(|generation| generation
                                .checked_add(u64::from(self.import_image.is_some())))),
            "mixed job source identity"
        );
        if let Some(edit) = &self.edit {
            ensure!(
                !self.import
                    && same_pixels(&edit.source, &self.expected)
                    && self
                        .request
                        .keys
                        .iter()
                        .all(|key| key.variant_id == edit.key.variant_id
                            && key.edit_revision == edit.revision as u64
                            && key.image_pixel_generation
                                == edit
                                    .image_identity
                                    .as_ref()
                                    .map(|image| image.pixel_generation as u64)),
                "mixed job edit identity"
            );
            if let Some(work) = &self.request.edit {
                ensure!(
                    work.recipe_digest == edit.recipe_digest,
                    "job recipe authority differs"
                );
            } else {
                ensure!(
                    edit.key.variant_id == MASTER && edit.revision == 0,
                    "missing job recipe"
                );
            }
        } else {
            ensure!(self.request.edit.is_none(), "missing job edit authority");
            ensure!(
                self.request
                    .keys
                    .iter()
                    .all(|key| key.image_pixel_generation
                        == self
                            .import_image
                            .as_ref()
                            .and_then(|edit| edit.image_identity.as_ref())
                            .map(|image| image.pixel_generation as u64)),
                "missing image pixel authority"
            );
        }
        Ok(())
    }
    fn current(&self, catalog: &Catalog) -> Result<bool> {
        if let Some(edit) = &self.edit {
            Ok(same_edit(edit, &current_edit(catalog, edit)?))
        } else {
            ensure!(
                self.request.edit.is_none()
                    && self
                        .request
                        .keys
                        .iter()
                        .all(|key| key.variant_id == MASTER && key.edit_revision == 0),
                "missing edit authority"
            );
            let current = catalog.render_identity(&self.expected.asset_id)?;
            if !same_pixels(&current, &self.expected) {
                return Ok(false);
            }
            if let Some(image) = &self.import_image {
                return Ok(same_edit(image, &current_edit(catalog, image)?));
            }
            Ok(self.import
                || catalog
                    .edit_render_identity(&VariantKey::master(&self.expected.asset_id))?
                    .revision
                    == 0)
        }
    }
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
        let prepared = super::prepared_cache::PreparedCache::open(
            store.configuration().manifest_root.join("prepared"),
            limits.prepared_cache_bytes,
            limits.prepared_cache_entries,
        )?;
        Ok(Self {
            launch_pauses: Default::default(),
            prepared,
            reads: read_queue::ReadQueue::default(),
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
            worker_metrics: None,
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
    /// Bounded latest-result diagnostics, consumed once. Multiple workers may
    /// replace an earlier result within one tick; this is not an audit log.
    pub fn take_worker_metrics(&mut self) -> Option<WorkerResourceMetrics> {
        self.worker_metrics.take()
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
            image_pixel_generation: None,
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
    pub fn variant_key(&self, identity: &EditRenderIdentity, tier: Tier) -> Result<PreviewKey> {
        let mut key = self.key(
            &identity.source,
            tier,
            identity
                .source
                .fingerprint
                .as_deref()
                .unwrap_or(&"0".repeat(64)),
        )?;
        key.variant_id = identity.key.variant_id.clone();
        key.image_pixel_generation = identity
            .image_identity
            .as_ref()
            .map(|image| u64::try_from(image.pixel_generation))
            .transpose()?;
        key.edit_revision = u64::try_from(identity.revision)?;
        if identity.key.variant_id != MASTER || identity.revision != 0 {
            key.renderer_version = super::worker::edited_renderer(false);
        }
        key.validate()?;
        Ok(key)
    }
    // The pending→ready UPDATE advances physical_generation exactly once.
    // The catalog callback verifies that resulting generation before attachment.
    fn import_key(
        &self,
        identity: &EditRenderIdentity,
        tier: Tier,
        fingerprint: &str,
    ) -> Result<PreviewKey> {
        let mut key = self.variant_key(identity, tier)?;
        key.generation = key
            .generation
            .checked_add(1)
            .context("import physical generation overflow")?;
        key.fingerprint = fingerprint.into();
        key.validate()?;
        Ok(key)
    }
    pub fn interactive_key(&self, identity: &EditRenderIdentity, tier: Tier) -> Result<PreviewKey> {
        let mut key = self.variant_key(identity, tier)?;
        ensure!(
            key.edge <= super::prepared_cache::PROXY_EDGE,
            "interactive preview exceeds1600 source proxy"
        );
        key.renderer_version = super::worker::edited_renderer(true);
        Ok(key)
    }
    pub fn cached_interactive(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
    ) -> Result<Option<PreviewView>> {
        self.cached_inner(catalog, variant, tier, allow_stale, true, None)
    }
    pub fn cached_variant(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
    ) -> Result<Option<PreviewView>> {
        self.cached_inner(catalog, variant, tier, allow_stale, false, None)
    }
    pub fn cached(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        tier: Tier,
        allow_stale: bool,
    ) -> Result<Option<PreviewView>> {
        self.cached_inner(
            catalog,
            &VariantKey::master(asset),
            tier,
            allow_stale,
            false,
            None,
        )
    }
    /// Same production path with opt-in phase clocks. Timings remain available
    /// on errors/misses; instrumentation overhead is included, never subtracted.
    pub fn cached_with_metrics(
        &mut self,
        catalog: &Catalog,
        asset: &str,
        tier: Tier,
        allow_stale: bool,
        metrics: &mut CacheReadMetrics,
    ) -> Result<Option<PreviewView>> {
        self.cached_variant_with_metrics(
            catalog,
            &VariantKey::master(asset),
            tier,
            allow_stale,
            metrics,
        )
    }
    pub fn cached_variant_with_metrics(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
        metrics: &mut CacheReadMetrics,
    ) -> Result<Option<PreviewView>> {
        *metrics = CacheReadMetrics::default();
        let before = self.decoded.access_counts();
        let start = Instant::now();
        let result = self.cached_inner(
            catalog,
            variant,
            tier,
            allow_stale,
            false,
            Some(&mut *metrics),
        );
        metrics.total_ms = start.elapsed().as_secs_f64() * 1000.0;
        let after = self.decoded.access_counts();
        metrics.decoded_hits = after.0.saturating_sub(before.0);
        metrics.decoded_misses = after.1.saturating_sub(before.1);
        metrics.returned_pixels = matches!(&result, Ok(Some(_)));
        result
    }
    fn cached_inner(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
        interactive: bool,
        mut metrics: Option<&mut CacheReadMetrics>,
    ) -> Result<Option<PreviewView>> {
        let identity = measured(&mut metrics, ReadPhase::Identity, || {
            catalog.edit_render_identity(variant)
        })?;
        let key = if interactive {
            self.interactive_key(&identity, tier)?
        } else {
            self.variant_key(&identity, tier)?
        };
        let allowance = self.limits.encoded_staging_bytes - self.encoded.used();
        let _reservation = self
            .encoded
            .try_reserve(allowance)
            .ok_or(EncodedBudgetExceeded)?;
        let Some(cached) = measured(&mut metrics, ReadPhase::Store, || {
            if let Some(cached) = self.store.read_limited(&key, false, allowance)? {
                return Ok(Some(cached));
            }
            let mut legacy = identity.clone();
            legacy.source = catalog.render_identity(&variant.asset_id)?;
            legacy.image_identity = None;
            let legacy_key = if interactive {
                self.interactive_key(&legacy, tier)?
            } else {
                self.variant_key(&legacy, tier)?
            };
            if let Some(cached) = self.store.read_limited(&legacy_key, false, allowance)? {
                return Ok(Some(cached));
            }
            self.store.read_limited(&key, allow_stale, allowance)
        })?
        else {
            if allow_stale
                && variant.variant_id == MASTER
                && tier == Tier::Thumbnail
                && let Some((hash, bytes)) = measured(&mut metrics, ReadPhase::Store, || {
                    catalog.retained_legacy_preview(&variant.asset_id, allowance)
                })?
            {
                let pixels = measured(&mut metrics, ReadPhase::Decode, || {
                    let (width, height) = encoded_dimensions(&bytes, Codec::Jpeg)?;
                    self.decoded.decode(
                        blake3::hash(format!("legacy:{hash}").as_bytes())
                            .to_hex()
                            .to_string(),
                        &bytes,
                        Codec::Jpeg,
                        width,
                        height,
                    )
                })?;
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
        let pixels = measured(&mut metrics, ReadPhase::Decode, || -> Result<_> {
            let (width, height) = encoded_dimensions(&cached.bytes, cached.key.encoding.codec)?;
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
        });
        let pixels = match pixels {
            Ok(pixels) => pixels,
            Err(error) => {
                if error.downcast_ref::<DecodedBudgetExceeded>().is_none() {
                    self.store.invalidate(&cached.key)?;
                }
                return Err(error);
            }
        };
        let current = measured(&mut metrics, ReadPhase::Identity, || {
            catalog.edit_render_identity(variant)
        })?;
        let stale =
            cached.stale || !same_edit(&identity, &current) || current.source.state != "ready";
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
        self.encoded_cached_variant(
            catalog,
            &VariantKey::master(asset),
            tier,
            allow_stale,
            false,
        )
    }
    pub fn encoded_cached_variant(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
        interactive: bool,
    ) -> Result<Option<EncodedPreview>> {
        let view = if interactive {
            self.cached_interactive(catalog, variant, tier, allow_stale)?
        } else {
            self.cached_variant(catalog, variant, tier, allow_stale)?
        };
        let Some(view) = view else {
            return Ok(None);
        };
        let allowance = self.limits.encoded_staging_bytes - self.encoded.used();
        let reservation = self
            .encoded
            .try_reserve(allowance)
            .ok_or(EncodedBudgetExceeded)?;
        let bytes = if let Some(key) = view.key {
            self.store
                .read_limited(&key, false, allowance)?
                .context("cache changed during export")?
                .bytes
        } else {
            catalog
                .retained_legacy_preview(&variant.asset_id, allowance)?
                .context("legacy preview changed during export")?
                .1
        };
        Ok(Some(EncodedPreview {
            bytes,
            _reservation: reservation,
        }))
    }
    /// Validate actual source admission independently of configured root hints.
    pub(crate) fn ensure_original_separate(&self, source: &Path) -> Result<()> {
        self.store.ensure_original_separate(source)
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
        let import_image = catalog.edit_render_identity(&VariantKey::master(asset))?;
        ensure!(import_image.revision == 0, "pending import already edited");
        let key = self.import_key(&import_image, Tier::Thumbnail, fingerprint)?;
        self.submit(
            catalog,
            SavedJob {
                request: RenderWork {
                    edit: None,
                    source: NativePath::from_path(source),
                    keys: vec![key],
                    encoded_limit: self.limits.per_worker_encoded_bytes,
                    decode_limits: self.limits.decode_limits,
                },
                expected,
                edit: None,
                import_image: Some(import_image),
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
        self.request_variant(catalog, &VariantKey::master(asset), tier, priority)
    }
    pub fn request_variant(
        &mut self,
        catalog: &mut Catalog,
        variant: &VariantKey,
        tier: Tier,
        priority: Priority,
    ) -> Result<Consumer> {
        self.request_variant_mode(catalog, variant, tier, priority, false)
    }
    pub fn request_interactive(
        &mut self,
        catalog: &mut Catalog,
        variant: &VariantKey,
        tier: Tier,
        priority: Priority,
    ) -> Result<Consumer> {
        self.request_variant_mode(catalog, variant, tier, priority, true)
    }
    fn request_variant_mode(
        &mut self,
        catalog: &mut Catalog,
        variant: &VariantKey,
        tier: Tier,
        priority: Priority,
        interactive: bool,
    ) -> Result<Consumer> {
        let expected = catalog.edit_render_identity(variant)?;
        ensure!(expected.source.state == "ready", "original is not ready");
        ensure!(
            expected.source.fingerprint.is_some(),
            "original fingerprint missing"
        );
        let view = catalog.edit_variant(variant)?;
        ensure!(
            view.revision == expected.revision && view.recipe_digest == expected.recipe_digest,
            "edit changed during request preparation"
        );
        let key = if interactive {
            self.interactive_key(&expected, tier)?
        } else {
            self.variant_key(&expected, tier)?
        };
        let edit = if !interactive && variant.variant_id == MASTER && expected.revision == 0 {
            None
        } else {
            Some(super::worker::EditWork {
                recipe: view.recipe,
                recipe_digest: expected.recipe_digest.clone(),
                limits: self.edit_limits(),
                interactive,
                prepared_bytes: if self.limits.prepared_cache_entries == 0
                    || self.limits.prepared_cache_bytes < super::prepared_cache::MAX_PROXY_BYTES
                {
                    0
                } else {
                    super::prepared_cache::MAX_PROXY_BYTES
                },
                prepared: None,
            })
        };
        let source = catalog.preview_original_path(&variant.asset_id)?;
        self.submit(
            catalog,
            SavedJob {
                request: RenderWork {
                    source,
                    keys: vec![key],
                    encoded_limit: self.limits.per_worker_encoded_bytes,
                    decode_limits: self.limits.decode_limits,
                    edit,
                },
                expected: expected.source.clone(),
                edit: Some(expected),
                import_image: None,
                import: false,
                state: JobState::Queued,
            },
            priority,
        )
    }
    fn edit_limits(&self) -> crate::edit::RenderLimits {
        crate::edit::RenderLimits {
            max_pixels: self.limits.decode_limits.max_intermediate_pixels,
            max_allocation_bytes: self.limits.decode_limits.max_allocation_bytes,
            max_live_bytes: self.limits.per_worker_bytes,
        }
    }
    fn submit(
        &mut self,
        catalog: &mut Catalog,
        job: SavedJob,
        priority: Priority,
    ) -> Result<Consumer> {
        job.validate()?;
        job.request.validate()?;
        self.ensure_original_separate(&job.request.source.to_path()?)?;
        ensure!(self.available_request_slots() > 0, "preview consumer limit");
        let id = blake3::hash(&serde_json::to_vec(&job.request.keys)?)
            .to_hex()
            .to_string();
        let stored = serde_json::to_string(&job)?;
        let writer_priority = match priority {
            Priority::Foreground => crate::catalog_writer::Priority::Foreground,
            Priority::Background => crate::catalog_writer::Priority::Background,
        };
        let persist = || {
            self.store.save_job(&id, &stored, self.limits.requests)?;
            for key in &job.request.keys {
                self.store.desire(key, || Ok(true))?;
            }
            Ok(())
        };
        let admitted = if let Some(edit) = &job.edit {
            with_preview_transaction(catalog, edit, writer_priority, |_| persist())?
        } else {
            catalog.with_render_transaction(&job.expected, writer_priority, |tx| {
                ensure!(
                    import_image_current(tx, job.import_image.as_ref(), false)?,
                    "stale import pixel identity"
                );
                persist()
            })?
        };
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
            let active = &self.active[&lease_id];
            let current = self
                .jobs
                .get(&active.lease.key)
                .context("active job missing")?
                .current(catalog)?;
            if !current {
                active
                    .lease
                    .canceled
                    .store(true, std::sync::atomic::Ordering::Release);
            }
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
            let publication = if !current {
                Ok(ServiceCompletion::Stale)
            } else if canceled {
                Ok(ServiceCompletion::Canceled)
            } else {
                result.and_then(|batch| {
                    let batch = batch.context("missing worker result")?;
                    self.worker_metrics = Some(WorkerResourceMetrics {
                        pid: active.worker.pid(),
                        keys: job.request.keys.clone(),
                        peak_resident_bytes: batch.peak_resident_bytes,
                        peak_method: batch.peak_method.clone(),
                    });
                    if let Some(prepared) = &batch.prepared {
                        self.prepared
                            .adopt(job.request.keys[0].generation, prepared)?;
                    }
                    self.publish_batch(catalog, &job, batch)
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
        if self.store.relocation_pending()?
            || self
                .launch_pauses
                .load(std::sync::atomic::Ordering::Acquire)
                != 0
        {
            return Ok(());
        }
        while self
            .encoded
            .used()
            .checked_add(self.limits.per_worker_encoded_bytes)
            .is_some_and(|used| used <= self.limits.encoded_staging_bytes / 2)
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
            if !job.current(catalog)? {
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
                .ok_or(EncodedBudgetExceeded)?;
            let launch = (|| {
                let source = job.request.source.to_path()?;
                self.ensure_original_separate(&source)?;
                let mut request = job.request.clone();
                if let Some(edit) = &mut request.edit
                    && edit.interactive
                {
                    edit.prepared = self.prepared.lookup(
                        &source,
                        request.keys[0].generation,
                        &request.keys[0].fingerprint,
                        &edit.recipe.validate()?.settings().white_balance,
                    )?;
                }
                WorkerProcess::spawn(&self.executable, &self.staging, request)
            })();
            match launch {
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
                edit_input: batch.edit_input.clone(),
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
                            catalog.commit_preview_import_guarded(
                                &job.expected,
                                &object.key.fingerprint,
                                &batch.metadata,
                                &object.key.digest()?,
                                (
                                    |tx, published| {
                                        import_image_current(
                                            tx,
                                            job.import_image.as_ref(),
                                            published,
                                        )
                                    },
                                    || {
                                        let publication = attach()?;
                                        self.observe(ServiceEvent::ManifestAttached)?;
                                        Ok(publication)
                                    },
                                ),
                                || self.observe(ServiceEvent::BeforeCatalogCommit),
                            )?
                        } else if let Some(edit) = &job.edit {
                            with_preview_transaction(
                                catalog,
                                edit,
                                crate::catalog_writer::Priority::Foreground,
                                |_| attach(),
                            )?
                        } else {
                            let mut edit = catalog.edit_render_identity(&VariantKey::master(
                                &job.expected.asset_id,
                            ))?;
                            // Historical unscoped jobs use the legacy generation.
                            edit.source = catalog.render_identity(&job.expected.asset_id)?;
                            edit.image_identity = None;
                            if edit.revision != 0 || !same_pixels(&edit.source, &job.expected) {
                                None
                            } else {
                                with_preview_transaction(
                                    catalog,
                                    &edit,
                                    crate::catalog_writer::Priority::Foreground,
                                    |_| attach(),
                                )?
                            }
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
            || error.downcast_ref::<EncodedBudgetExceeded>().is_some()
            || error.downcast_ref::<DecodedBudgetExceeded>().is_some()
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
                if self.available_request_slots() == 0 {
                    break;
                }
                cursor = position;
                if self.jobs.contains_key(&id) {
                    continue;
                }
                let mut job: SavedJob = serde_json::from_str(&descriptor)?;
                job.validate()?;
                // A persisted legacy journal cannot downgrade a newer scoped request.
                // Retire only this obsolete journal; desired/current objects stay intact.
                let mut superseded = false;
                for key in &job.request.keys {
                    superseded |=
                        key.image_pixel_generation.is_none() && self.store.scoped_desired(key)?;
                }
                if superseded {
                    self.store.finish_job(&id)?;
                    continue;
                }
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
                if !job.current(catalog)? {
                    self.store.finish_job(&id)?;
                    continue;
                }
                // Crash after manifest attachment but before catalog ready commit.
                if job.import
                    && self.store.current_is_intact(key)?
                    && let Some(record) = self.store.render_record(key)?
                    && catalog
                        .commit_preview_import_guarded(
                            &job.expected,
                            &key.fingerprint,
                            &record.metadata,
                            &key.digest()?,
                            (
                                |tx, published| {
                                    import_image_current(tx, job.import_image.as_ref(), published)
                                },
                                || Ok(()),
                            ),
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
                    .map(|key| {
                        if let Some(edit) = &job.edit {
                            if job
                                .request
                                .edit
                                .as_ref()
                                .is_some_and(|work| work.interactive)
                            {
                                self.interactive_key(edit, key.tier)
                            } else {
                                self.variant_key(edit, key.tier)
                            }
                        } else if let Some(image) = &job.import_image {
                            self.import_key(image, key.tier, &fingerprint)
                        } else {
                            self.key(&current, key.tier, &fingerprint)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                job.request.source = catalog.preview_original_path(&job.expected.asset_id)?;
                job.request.decode_limits = self.limits.decode_limits;
                if let Some(edit) = &mut job.request.edit {
                    edit.limits = self.edit_limits();
                    edit.prepared_bytes = if self.limits.prepared_cache_entries == 0
                        || self.limits.prepared_cache_bytes < super::prepared_cache::MAX_PROXY_BYTES
                    {
                        0
                    } else {
                        super::prepared_cache::MAX_PROXY_BYTES
                    };
                    edit.prepared = None;
                }
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
        self.reads.len() == 0
            && self.consumers.is_empty()
            && self.active.is_empty()
            && self.scheduler.usage().queued == 0
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
        self.limits
            .requests
            .saturating_sub(self.consumers.len() + self.reads.len())
    }
    pub fn pause_native_launches(&self) -> Result<NativeLaunchPause> {
        self.launch_pauses
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| n.checked_add(1),
            )
            .map_err(|_| anyhow::anyhow!("native pause counter overflow"))?;
        Ok(NativeLaunchPause(self.launch_pauses.clone()))
    }
    /// Queued descriptors may remain while paused; only active children and
    /// their reservations must drain before the owner admits an external worker.
    pub fn native_work_drained(&self) -> bool {
        self.active.is_empty() && self.scheduler.usage().reserved_bytes == 0
    }
    /// Bounded by configured workers. These are owned, not-yet-reaped process
    /// IDs; callers must observe OS liveness separately and account for PID reuse.
    pub fn active_worker_pids(&self) -> Vec<u32> {
        let mut pids: Vec<_> = self.active.values().map(|job| job.worker.pid()).collect();
        pids.sort_unstable();
        pids
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

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod recovery_tests;
