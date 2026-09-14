//! One header-upgrade candidate shares the renderer's scheduler and budgets.
use super::*;
use crate::{application::U64, catalog_session::native as n};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) enum Begin {
    Ready(Option<Box<PreviewView>>),
    Pending(Box<ManagedRead>),
}
pub(super) struct ManagedRead {
    identity: EditRenderIdentity,
    tier: Tier,
    allow_stale: bool,
    key: Option<PreviewKey>,
    legacy_hash: Option<String>,
    record: Option<RenderRecord>,
    stale: bool,
    cache_key: String,
    codec: Codec,
    expected: Option<(u32, u32)>,
    encoded: Vec<u8>,
    encoded_task: Option<
        crate::preview::transport_task::Task<(
            crate::catalog_session::preview_io::Integrity,
            Vec<u8>,
        )>,
    >,
    _encoded: ByteReservation,
    encoded_allowance: u64,
    pub(super) preempting: bool,
    cost: Option<u64>,
    pub(super) request: Option<super::super::scheduler::NativeReadRequest>,
    lease: Option<u64>,
    transport: super::super::worker::read_transport::Read,
    rgb: Option<ByteReservation>,
    header: Option<n::Header>,
    requeue: Option<(u64, (u32, u32))>,
    launched: bool,
    cleanup_started: bool,
    pub cancel: Arc<AtomicBool>,
    failure: Option<anyhow::Error>,
}
/// SQL-only reference selection. File custody and bytes stay in F.
fn legacy_reference(catalog: &Catalog, asset: &str) -> Result<Option<String>> {
    let mut statement = catalog
        .db
        .prepare("SELECT preview_hash FROM assets WHERE id=?1")?;
    let mut rows = statement.query([asset])?;
    let row = rows.next()?.context("legacy asset missing")?;
    match row.get_ref(0)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        rusqlite::types::ValueRef::Text(bytes) => {
            ensure!(
                bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit),
                "invalid legacy preview reference"
            );
            Ok(Some(std::str::from_utf8(bytes)?.to_owned()))
        }
        _ => anyhow::bail!("invalid legacy preview reference storage class"),
    }
}
impl PreviewService {
    pub(super) fn begin_managed_read(
        &mut self,
        catalog: &Catalog,
        variant: &VariantKey,
        tier: Tier,
        allow_stale: bool,
        interactive: bool,
        synchronous: bool,
    ) -> Result<Begin> {
        let identity = catalog.edit_render_identity(variant)?;
        let wanted = if interactive {
            self.interactive_key(&identity, tier)?
        } else {
            self.variant_key(&identity, tier)?
        };
        let allowance = self.limits.encoded_staging_bytes - self.encoded.used();
        let mut guard = self
            .encoded
            .try_reserve(allowance)
            .ok_or(EncodedBudgetExceeded)
            .with_context(|| {
                format!(
                    "managed read staging admission: requested {allowance}, retained {}, limit {}",
                    self.encoded.used(),
                    self.limits.encoded_staging_bytes
                )
            })?;
        let mut cached = self.store.select_managed_read(&wanted, false, allowance)?;
        if cached.is_none() {
            let source = catalog.render_identity(&variant.asset_id)?;
            if legacy_pixel_eligible(&identity, &source) {
                let mut legacy = identity.clone();
                legacy.source = source;
                legacy.image_identity = None;
                let key = if interactive {
                    self.interactive_key(&legacy, tier)?
                } else {
                    self.variant_key(&legacy, tier)?
                };
                cached = self.store.select_managed_read(&key, false, allowance)?;
            }
        }
        if cached.is_none() {
            cached = self
                .store
                .select_managed_read(&wanted, allow_stale, allowance)?;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut selected = None;
        let (key, legacy_hash, record, stale, cache_key, codec, expected, bytes) =
            if let Some(selection) = cached {
                let cached = selection.cached;
                selected = Some((selection.files, selection.expected));
                let expected = cached.record.as_ref().map(|r| (r.width, r.height));
                let digest = cached.key.digest()?;
                let codec = cached.key.encoding.codec;
                (
                    Some(cached.key),
                    None,
                    cached.record,
                    cached.stale,
                    digest,
                    codec,
                    expected,
                    cached.bytes,
                )
            } else if allow_stale && variant.variant_id == MASTER && tier == Tier::Thumbnail {
                let Some(hash) = legacy_reference(catalog, &variant.asset_id)? else {
                    return Ok(Begin::Ready(None));
                };
                let cache_key = blake3::hash(format!("legacy:{hash}").as_bytes())
                    .to_hex()
                    .to_string();
                (
                    None,
                    Some(hash),
                    None,
                    true,
                    cache_key,
                    Codec::Jpeg,
                    None,
                    Vec::new(),
                )
            } else {
                return Ok(Begin::Ready(None));
            };
        if legacy_hash.is_none()
            && let Some(pixels) = self.decoded.managed_lookup(&cache_key)?
        {
            let current = catalog.edit_render_identity(variant)?;
            let stale = stale || !same_edit(&identity, &current) || current.source.state != "ready";
            if stale && !(allow_stale && tier == Tier::Thumbnail) {
                return Ok(Begin::Ready(None));
            }
            return Ok(Begin::Ready(Some(Box::new(PreviewView {
                key,
                legacy_hash,
                record,
                pixels,
                stale,
            }))));
        }
        if synchronous {
            self.ensure_synchronous_read_available()?;
        }
        let calls = self
            .store
            .stage_calls()
            .context("managed read filesystem missing")?;
        let transport =
            super::super::worker::read_transport::Read::new(calls.task_lane()?, cancel.clone());
        let allowance = if legacy_hash.is_some() {
            allowance.min(crate::catalog_session::preview_stage::LEGACY_BYTES)
        } else {
            allowance
        };
        guard.shrink(allowance)?;
        let encoded_task = if let Some((files, expected)) = selected {
            Some(crate::preview::transport_task::Task::spawn(
                "preview-cache-transfer",
                cancel.clone(),
                move |cancel| files.cache_read_cancel(expected, allowance, &cancel),
            )?)
        } else if let Some(hash) = legacy_hash.as_ref() {
            Some(transport.legacy_read(hash.clone(), allowance)?)
        } else {
            None
        };
        Ok(Begin::Pending(Box::new(ManagedRead {
            identity,
            tier,
            allow_stale,
            key,
            legacy_hash,
            record,
            stale,
            cache_key,
            codec,
            expected,
            encoded: bytes,
            encoded_task,
            _encoded: guard,
            encoded_allowance: allowance,
            preempting: false,
            cost: None,
            request: None,
            lease: None,
            transport,
            rgb: None,
            header: None,
            requeue: None,
            launched: false,
            cleanup_started: false,
            cancel,
            failure: None,
        })))
    }
    pub(super) fn poll_managed_read(
        &mut self,
        catalog: &Catalog,
        read: &mut ManagedRead,
    ) -> Result<Option<Option<PreviewView>>> {
        if read
            .request
            .as_ref()
            .is_some_and(|r| r.preempted.load(Ordering::Acquire))
            && !read.cancel.load(Ordering::Acquire)
        {
            read.preempt();
        }
        match self.advance_managed_read(catalog, read) {
            Ok(result) => Ok(result),
            Err(error) => {
                if read.cancel.load(Ordering::Acquire)
                    && error
                        .downcast_ref::<crate::filesystem_worker::wire::Failure>()
                        .is_some_and(|f| {
                            f.kind == crate::filesystem_worker::wire::FailureKind::Canceled
                        })
                {
                    return Ok(None);
                }
                read.preempting = false;
                if read.cleanup_started
                    && !read.transport.pending()
                    && read.lease.is_none()
                    && read.request.is_none()
                {
                    return Err(error);
                }
                if (error
                    .downcast_ref::<crate::media::DecodeError>()
                    .is_some_and(|e| e.status == crate::media::DecodeStatus::Corrupt)
                    || error.downcast_ref::<WorkerFailure>().is_some_and(|e| {
                        e.decode_status == Some(crate::media::DecodeStatus::Corrupt)
                    }))
                    && let Some(key) = &read.key
                {
                    let _ = self.store.finish_managed_read(
                        key,
                        crate::catalog_session::preview_io::Integrity::Corrupt,
                    );
                }
                read.failure.get_or_insert(error);
                read.cancel.store(true, Ordering::Release);
                Ok(None)
            }
        }
    }
    fn advance_managed_read(
        &mut self,
        catalog: &Catalog,
        read: &mut ManagedRead,
    ) -> Result<Option<Option<PreviewView>>> {
        use super::super::worker::read_transport::Event;
        if let Some(task) = &mut read.encoded_task {
            match task.poll() {
                Ok(None) => return Ok(None),
                Ok(Some((integrity, bytes))) => {
                    read.encoded_task = None;
                    let intact = if let Some(key) = &read.key {
                        self.store.finish_managed_read(key, integrity)?
                    } else {
                        integrity == crate::catalog_session::preview_io::Integrity::Intact
                    };
                    if !intact {
                        if read.key.is_some()
                            && !read.cancel.load(Ordering::Acquire)
                            && read.allow_stale
                            && read.identity.key.variant_id == MASTER
                            && read.tier == Tier::Thumbnail
                            && let Some(hash) =
                                legacy_reference(catalog, &read.identity.key.asset_id)?
                        {
                            read.cache_key = blake3::hash(format!("legacy:{hash}").as_bytes())
                                .to_hex()
                                .to_string();
                            read.key = None;
                            read.record = None;
                            read.legacy_hash = Some(hash.clone());
                            read.codec = Codec::Jpeg;
                            read.expected = None;
                            read.stale = true;
                            read.encoded_allowance = read
                                .encoded_allowance
                                .min(crate::catalog_session::preview_stage::LEGACY_BYTES);
                            read._encoded.shrink(read.encoded_allowance)?;
                            read.encoded_task =
                                Some(read.transport.legacy_read(hash, read.encoded_allowance)?);
                            return Ok(None);
                        }
                        read.cancel.store(true, Ordering::Release);
                    } else {
                        read.encoded = bytes;
                    }
                    read._encoded
                        .shrink(u64::try_from(read.encoded.capacity())?)?;
                    // Legacy compatibility still verifies its retained file before
                    // serving an existing decoded Arc; no N is required on a hit.
                    if read.legacy_hash.is_some()
                        && !read.cancel.load(Ordering::Acquire)
                        && let Some(pixels) = self.decoded.managed_lookup(&read.cache_key)?
                    {
                        return Ok(Some(Some(PreviewView {
                            key: None,
                            legacy_hash: read.legacy_hash.clone(),
                            record: None,
                            pixels,
                            stale: true,
                        })));
                    }
                }
                Err(error) => {
                    read.encoded_task = None;
                    return Err(error);
                }
            }
        }
        if read.cancel.load(Ordering::Acquire) {
            read.transport.signal_cancel();
            if read.transport.pending() {
                match read.transport.poll() {
                    Ok(None) => return Ok(None),
                    Ok(Some(Err(error))) | Err(error) => {
                        read.cleanup_started = false;
                        return Err(error);
                    }
                    _ => {}
                }
            }
            if !read.cleanup_started {
                read.transport.stop(true)?;
                read.cleanup_started = true;
                return Ok(None);
            }
            if read.transport.pending() {
                return Ok(None);
            }
            read.release_scheduler(&mut self.scheduler)?;
            read.rgb.take();
            return match read.failure.take() {
                Some(error) => Err(error),
                None => Ok(Some(None)),
            };
        }
        if read.request.is_none() {
            read.request = Some(self.scheduler.queue_native_read(Priority::Foreground)?);
        }
        if read.cost.is_none() {
            let e = u64::try_from(read.encoded.len())?;
            read.cost = Some(match read.expected {
                Some((w, h)) => n::decode_cost(
                    read.codec,
                    w,
                    h,
                    e,
                    self.limits.cache_header_scratch_bytes,
                    self.limits.cache_codec_scratch_bytes,
                )?,
                None => n::header_cost(e, self.limits.cache_header_scratch_bytes)?,
            });
        }
        if read.lease.is_none() {
            if self.launch_pauses.load(Ordering::Acquire) > 0
                || self.external_native.load(Ordering::Acquire)
            {
                return Ok(None);
            }
            let Some(lease) = self
                .scheduler
                .admit_native_read(read.request.as_ref().unwrap().id, read.cost.unwrap())?
            else {
                return Ok(None);
            };
            read.lease = Some(lease);
            if let Some((cost, dimensions)) = read.requeue.take() {
                read.transport.rearm(cost, dimensions)?;
                read.launched = true;
            }
        }
        if !read.launched {
            let work = n::Work::DecodeEncoded {
                codec: read.codec,
                encoded_bytes: U64(read.encoded.len() as u64),
                encoded_digest: blake3::hash(&read.encoded).to_hex().to_string(),
                expected_dimensions: read.expected,
            };
            read.transport.header(
                work,
                self.limits.clone(),
                read.cost.unwrap(),
                std::mem::take(&mut read.encoded),
            )?;
            read.launched = true;
        }
        let Some(event) = read.transport.poll()? else {
            return Ok(None);
        };
        match event? {
            Event::Header(header) => {
                ensure!(
                    read.key
                        .as_ref()
                        .is_none_or(|k| header.width <= k.edge && header.height <= k.edge),
                    crate::media::DecodeError {
                        status: crate::media::DecodeStatus::Corrupt,
                        message: "cached image exceeds tier".into()
                    }
                );
                let cost = n::decode_cost(
                    header.codec,
                    header.width,
                    header.height,
                    header.input_bytes.0,
                    self.limits.cache_header_scratch_bytes,
                    self.limits.cache_codec_scratch_bytes,
                )?;
                if !self
                    .scheduler
                    .upgrade_native_read(read.lease.unwrap(), cost)?
                {
                    read.requeue = Some((cost, (header.width, header.height)));
                    read.transport.stop(false)?;
                    return Ok(None);
                }
                read.cost = Some(cost);
                read.rgb = Some(self.decoded.reserve_rgb(header.width, header.height)?);
                read.header = Some(header.clone());
                read.transport.finish(header, cost)?;
                Ok(None)
            }
            Event::Drained => {
                if let Some((cost, dimensions)) = read.requeue {
                    self.scheduler
                        .requeue_native_read(read.lease.take().unwrap(), cost)?;
                    read.cost = Some(cost);
                    read.expected = Some(dimensions);
                    read.header = None;
                    read.rgb.take();
                    return Ok(None);
                }
                read.release_scheduler(&mut self.scheduler)?;
                Ok(Some(None))
            }
            Event::Pixels(pixels) => {
                let pixels = self.decoded.commit_rgb(
                    read.cache_key.clone(),
                    pixels,
                    read.rgb.take().context("decoded RGB reservation missing")?,
                )?;
                read.release_scheduler(&mut self.scheduler)?;
                let current = catalog.edit_render_identity(&read.identity.key)?;
                let stale = read.stale
                    || !same_edit(&read.identity, &current)
                    || current.source.state != "ready";
                if stale && !(read.allow_stale && read.tier == Tier::Thumbnail) {
                    return Ok(Some(None));
                }
                Ok(Some(Some(PreviewView {
                    key: read.key.clone(),
                    legacy_hash: read.legacy_hash.clone(),
                    record: read.record.clone(),
                    pixels,
                    stale,
                })))
            }
        }
    }
}
impl ManagedRead {
    pub(super) fn transport_busy(&self) -> bool {
        self.encoded_task
            .as_ref()
            .is_some_and(crate::preview::transport_task::Task::running)
            || self.transport.busy()
    }
    pub(super) fn signal_cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.transport.signal_cancel();
    }
    pub(super) fn preempt(&mut self) {
        self.preempting = true;
        self.signal_cancel();
    }
    pub(super) fn release_scheduler(&mut self, scheduler: &mut PreviewScheduler) -> Result<()> {
        if self.preempting {
            if let Some(lease) = self.lease.take() {
                scheduler.requeue_native_read(
                    lease,
                    self.cost.context("preempted read cost missing")?,
                )?;
            }
            return Ok(());
        }
        if let Some(lease) = self.lease.take() {
            scheduler.release_native_read(lease)?;
        } else if let Some(request) = &self.request {
            scheduler.cancel_queued_native_read(request.id)?;
        }
        self.request = None;
        Ok(())
    }
    pub(super) fn drain(&mut self) -> Result<bool> {
        self.preempting = false;
        self.cancel.store(true, Ordering::Release);
        if let Some(task) = &mut self.encoded_task {
            let _ = task.shutdown()?;
        }
        self.encoded_task = None;
        self.transport.shutdown()?;
        Ok(true)
    }
}
