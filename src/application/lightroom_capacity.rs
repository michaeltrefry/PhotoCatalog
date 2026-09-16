//! Checked Rust-backing admission for one managed Workbench generation.
//!
//! The Source reader payload allocator remains a different pool. This module
//! accounts the G/W/F metadata graph and transfers a subgrant from C's single
//! process reservation without charging that process reservation twice.

use super::{Config, lightroom_process};
use crate::preview::{ByteBudget, ByteReservation};
use anyhow::{Context, Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};

const FAILURE_BYTES: u64 = 4 * super::lightroom_managed::FAILURE_CHARS as u64;
const IDENTITY_BYTES: u64 = 128;
const DIGEST_BYTES: u64 = 64;
const ROSTER_LIMIT: u64 = crate::lightroom::selection::APPROVAL_ROSTER_LIMIT as u64;
pub(crate) const SOURCE_PAYLOAD_POOL_CONTRACT: &str = "distinct atomically retained Source high-water grant; nested reservations use its exact private counter";

pub(crate) fn source_requirement() -> Result<u64> {
    u64::try_from(
        crate::lightroom_migration_worker::source_reader::managed_workbench_requirement()?,
    )
    .context("Workbench Source requirement exceeds u64")
}

// The integrated dispatcher supplies actual compiler layouts; no parallel
// layout mirror can drift from its retained owner or queue entries.
pub(crate) fn dispatcher_layouts() -> [(usize, usize); 3] {
    super::desktop::workbench::metadata_layouts()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Retained,
    Active,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Contribution {
    pub name: &'static str,
    pub phase: Phase,
    pub group: &'static str,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub retained: u64,
    pub active: u64,
    pub required: u64,
    pub contributions: Vec<Contribution>,
}

#[derive(Clone, Copy)]
struct Checked;

impl Checked {
    fn value(self, value: u128) -> Result<u64> {
        ensure!(
            value <= isize::MAX as u128 && value <= u64::MAX as u128,
            "Workbench metadata capacity exceeds target isize"
        );
        Ok(value as u64)
    }

    fn add(self, values: &[u64]) -> Result<u64> {
        self.value(values.iter().try_fold(0u128, |sum, value| {
            sum.checked_add(u128::from(*value))
                .context("Workbench metadata addition overflow")
        })?)
    }

    fn mul(self, left: u64, right: u64) -> Result<u64> {
        self.value(
            u128::from(left)
                .checked_mul(u128::from(right))
                .context("Workbench metadata multiplication overflow")?,
        )
    }

    // Capacity growth may retain an old and a replacement allocation. This is
    // deliberately the same conservative vector rule as preview admission.
    fn vec(self, element: u64, potential: u64) -> Result<u64> {
        self.mul(self.add(&[self.mul(3, potential)?, 8])?, element)
    }

    fn string(self, bytes: u64) -> Result<u64> {
        self.vec(1, bytes)
    }

    fn native_path(self, units: u64) -> Result<u64> {
        self.vec(2, units)
    }

    fn json(self, raw: u64) -> Result<u64> {
        // serde Content has at most one node per two bytes for valid JSON. The
        // byte/string backing and node backing can overlap while materializing
        // a typed request/result.
        let nodes = self.add(&[raw, 1])? / 2;
        self.add(&[
            self.vec(1, raw)?,
            self.vec(
                std::mem::size_of::<serde::__private229::de::Content<'static>>() as u64,
                nodes,
            )?,
        ])
    }
}

struct Assembly {
    checked: Checked,
    contributions: Vec<Contribution>,
}

impl Assembly {
    fn new() -> Self {
        Self {
            checked: Checked,
            contributions: Vec::new(),
        }
    }

    fn push(&mut self, name: &'static str, phase: Phase, group: &'static str, bytes: u64) {
        self.contributions.push(Contribution {
            name,
            phase,
            group,
            bytes,
        });
    }

    fn finish(self) -> Result<Report> {
        // F retains failed subordinate custody independently, so every retained
        // contribution must fit together until checked reconciliation.
        let retained = self.checked.add(
            &self
                .contributions
                .iter()
                .filter(|value| value.phase == Phase::Retained)
                .map(|value| value.bytes)
                .collect::<Vec<_>>(),
        )?;
        // W's long operation thread can retain its request/selection documents
        // while the input loop serves control, result and F/S callbacks. Sum
        // those lifetimes; the single G dispatcher does not make them exclusive.
        let active = self.checked.add(
            &self
                .contributions
                .iter()
                .filter(|value| value.phase == Phase::Active)
                .map(|value| value.bytes)
                .collect::<Vec<_>>(),
        )?;
        Ok(Report {
            retained,
            active,
            required: self.checked.add(&[retained, active])?,
            contributions: self.contributions,
        })
    }
}

/// Return the full configured G/W/F backing. The dispatcher is one worker, but
/// every configured data queue slot and reserved control slot can retain both
/// an encoded envelope and its typed graph until delivery.
pub(crate) fn report(config: &Config, control_slots: usize) -> Result<Report> {
    let c = Checked;
    let mut a = Assembly::new();
    let queue = u64::try_from(config.limits.queued)?;
    let controls = u64::try_from(control_slots)?;
    let slots = c.add(&[queue, controls, 1])?;
    ensure!(queue > 0 && controls > 0, "Workbench dispatcher slot bound");

    let w = super::lightroom::metadata_layouts();
    let (w_channel, w_receiver) = super::lightroom::channel_metadata_layouts()?;
    let coordinator_channel = lightroom_process::coordinator_channel_backing()? as u64;
    let bridge = super::lightroom_bridge::metadata_layouts();
    let managed = super::lightroom_managed::metadata_layouts();
    let f = crate::filesystem_worker::lightroom_workbench_retained_metadata_layouts();
    let progress_control = crate::lightroom::control::metadata_allocation()?;
    let workbench_limits = super::lightroom::Limits::metadata_maximum();
    let selection_limits = crate::lightroom::selection::SelectionLimits::metadata_maximum();
    let (
        selection_review,
        selection_request,
        approval_document,
        approval_draft,
        approval_documents,
    ) = crate::lightroom::selection::metadata_layouts();
    let path = c.native_path(workbench_limits.native_path_units as u64)?;
    let manifest = workbench_limits.request_bytes as u64;
    let page = workbench_limits.page_bytes as u64;
    let result = workbench_limits.result_bytes as u64;
    let review = selection_limits.review_bytes as u64;
    let original = crate::filesystem_worker::wire::LIGHTROOM_ORIGINAL_BYTES;
    let callback = lightroom_process::CALLBACK_BYTES as u64;
    let artifacts = c.add(&[manifest, 1])? / 2;
    let evidence_construction =
        crate::filesystem_worker::lightroom_workbench_evidence_construction_layouts(
            usize::try_from(artifacts)?,
            usize::try_from(manifest)?,
        )?;
    let envelope = lightroom_process::ENVELOPE_BYTES as u64;
    let [dispatcher_layout, shared_layout, entry_layout] = dispatcher_layouts();
    let arc = |layout: (usize, usize)| -> Result<u64> {
        Ok(crate::lightroom_migration_worker::memory::channels::arc(
            std::alloc::Layout::from_size_align(layout.0, layout.1)?,
        )? as u64)
    };
    let [completion_layout, cancellation_layout] =
        super::desktop::workbench::completion_metadata_layouts();
    let cancellation_flag = arc((
        std::mem::size_of::<AtomicBool>(),
        std::mem::align_of::<AtomicBool>(),
    ))?;
    let completion_channel = super::desktop::workbench::completion_channel_backing()? as u64;
    let callback_layouts = lightroom_process::callback_metadata_layouts();
    let source = c.add(&[f.source as u64, path, c.mul(2, c.string(IDENTITY_BYTES)?)?])?;
    let source_budget = crate::preview::budget_state_layout();
    let source_budget = crate::lightroom_migration_worker::memory::channels::arc(
        std::alloc::Layout::from_size_align(source_budget.0, source_budget.1)
            .context("Source budget state layout")?,
    )? as u64;

    a.push(
        "g.dispatcher.fixed_owner_and_shared_layouts",
        Phase::Retained,
        "base",
        c.add(&[
            dispatcher_layout.0 as u64,
            arc(shared_layout)?,
            cancellation_flag,
        ])?,
    );
    a.push(
        "g.dispatcher.queued_and_active_typed_requests",
        Phase::Retained,
        "base",
        c.add(&[
            c.vec(entry_layout.0 as u64, queue)?,
            c.vec(entry_layout.0 as u64, controls)?,
            entry_layout.0 as u64,
            c.mul(slots, c.mul(2, c.json(envelope)?)?)?,
        ])?,
    );
    a.push(
        "g.dispatcher.completion_permits_channels_and_cancellation",
        Phase::Retained,
        "base",
        c.mul(
            slots,
            c.add(&[
                arc(completion_layout)?,
                std::mem::size_of::<std::sync::Arc<()>>() as u64,
                cancellation_flag,
                arc(cancellation_layout)?,
                completion_channel,
            ])?,
        )?,
    );
    a.push(
        "transient.managed_document_callback_roots",
        Phase::Active,
        "callback",
        c.add(&[
            callback_layouts.proxy as u64,
            callback_layouts.state as u64,
            callback_layouts.assembly as u64,
            callback_layouts.request as u64,
            callback_layouts.value as u64,
            callback_layouts.outcome as u64,
            c.add(&[
                callback_layouts.sealed_request as u64,
                callback_layouts.sealed_reply as u64,
            ])?
            .max(c.add(&[
                callback_layouts.artifact_request as u64,
                callback_layouts.artifact_reply as u64,
            ])?),
            c.json(callback)?,
            c.json(envelope)?,
        ])?,
    );
    a.push(
        "g.source_parent_and_private_budget_counters",
        Phase::Retained,
        "base",
        c.mul(2, source_budget)?,
    );
    a.push(
        "g.owner_generation_router_reader_custody",
        Phase::Retained,
        "base",
        c.add(&[
            managed.owner as u64,
            managed.generation as u64,
            managed.router as u64,
            managed.reader as u64,
            managed.retained_reader as u64,
            managed
                .sql_reader_backing
                .max(managed.capture_reader_backing) as u64,
            managed.custody as u64,
            managed.identity as u64,
            managed.resource as u64,
            FAILURE_BYTES,
            // Owner guard + router guard + one retained reader id + every
            // simultaneously retainable custody identity/resource string.
            c.mul(25, c.string(IDENTITY_BYTES)?)?,
        ])?,
    );
    a.push(
        "g.capability_pending_sessions_and_receipts",
        Phase::Retained,
        "base",
        c.add(&[
            managed.capability_custody as u64,
            // The fixed custody root contains the pending request and both
            // retained session request roots inline. Their independently owned
            // NativePath/String backings remain live together until checked F
            // reconciliation.
            c.mul(3, path)?,
            c.mul(3, c.string(IDENTITY_BYTES)?)?,
            c.mul(4, c.string(DIGEST_BYTES)?)?,
            // F caps prepared receipts at 4,096. G retains the exact same
            // bounded roster plus one cloned cleanup key while a discard call
            // is in flight; no hash/tree allocator or unbounded set is used.
            c.vec(
                managed.receipt as u64,
                super::lightroom_managed::CAPABILITY_RECEIPTS as u64,
            )?,
            c.mul(
                super::lightroom_managed::CAPABILITY_RECEIPTS as u64 + 1,
                c.string(IDENTITY_BYTES)?,
            )?,
        ])?,
    );
    a.push(
        "w.control_status_worker_and_bridge",
        Phase::Retained,
        "base",
        c.add(&[
            w.status as u64,
            w.cached as u64,
            w.shared as u64,
            w.message as u64,
            w.control as u64,
            w.workbench as u64,
            w.worker_owner as u64,
            w_channel as u64,
            w_receiver as u64,
            coordinator_channel,
            c.mul(2, c.json(lightroom_process::ENVELOPE_BYTES as u64)?)?,
            progress_control.retained_arcs as u64,
            bridge.control as u64,
            bridge.coordinator as u64,
            FAILURE_BYTES,
            c.mul(8, c.string(IDENTITY_BYTES)?)?,
            c.mul(3, path)?,
        ])?,
    );
    a.push(
        "transient.w_progress_control_installation",
        Phase::Active,
        "callback",
        progress_control.active()? as u64,
    );
    a.push(
        "w.upload_single_staged_input",
        Phase::Retained,
        "base",
        c.add(&[
            bridge.upload as u64,
            c.mul(3, manifest)?,
            super::lightroom_bridge::INPUT_OWNED_OVERHEAD as u64,
            c.json(manifest)?,
            c.mul(8, c.string(IDENTITY_BYTES)?)?,
        ])?,
    );
    a.push(
        "w.cached_result_and_review",
        Phase::Retained,
        "base",
        c.add(&[
            c.vec(1, result)?,
            selection_review as u64,
            c.vec(1, review)?,
            c.mul(ROSTER_LIMIT, approval_document as u64)?,
        ])?,
    );
    a.push(
        "f.owner_root",
        Phase::Retained,
        "root",
        c.add(&[
            f.owner as u64,
            f.root as u64,
            c.mul(4, f.release_receipt as u64)?,
            source,
            path,
            c.mul(19, c.string(IDENTITY_BYTES)?)?,
        ])?,
    );
    a.push(
        "f.capture_process_and_manifest",
        Phase::Retained,
        "capture",
        c.add(&[
            f.capture as u64,
            f.capture_process as u64,
            c.mul(4, path)?,
            c.mul(3, c.string(IDENTITY_BYTES)?)?,
            c.string(DIGEST_BYTES)?,
            c.vec(1, manifest)?,
            c.json(manifest)?,
        ])?,
    );
    a.push(
        "f.evidence_manifest_source_roster",
        Phase::Retained,
        "evidence",
        c.add(&[
            f.evidence as u64,
            c.mul(2, source)?,
            c.mul(16, c.string(IDENTITY_BYTES)?)?,
            c.mul(4, c.string(DIGEST_BYTES)?)?,
            evidence_construction.raw_vector as u64,
            c.mul(
                artifacts,
                c.add(&[
                    // The inline Source roots are in raw_vector. Each Source
                    // separately owns its path and two identity strings.
                    c.add(&[path, c.mul(2, c.string(IDENTITY_BYTES)?)?])?,
                    c.string(DIGEST_BYTES)?,
                    std::mem::size_of::<crate::lightroom_migration_worker::identity::FileKey>()
                        as u64,
                ])?,
            )?,
            c.vec(1, manifest)?,
            c.json(manifest)?,
        ])?,
    );
    a.push(
        "f.original_candidate_and_encoded_result",
        Phase::Retained,
        "original",
        c.add(&[
            f.original as u64,
            path,
            c.vec(1, original)?,
            c.mul(7, c.string(IDENTITY_BYTES)?)?,
            c.string(DIGEST_BYTES)?,
        ])?,
    );
    a.push(
        "f.seal_paths_uploads_and_digests",
        Phase::Retained,
        "seal",
        c.add(&[
            f.seal as u64,
            c.mul(f.seal_paths as u64, path)?,
            c.mul(10, c.string(IDENTITY_BYTES)?)?,
            c.mul(5, c.string(DIGEST_BYTES)?)?,
        ])?,
    );
    a.push(
        "transient.f_evidence_roster_construction",
        Phase::Active,
        "filesystem",
        c.add(&[
            evidence_construction.roster_tree as u64,
            evidence_construction.roster_strings as u64,
        ])?,
    );
    a.push(
        "transient.request_decode_and_typed_action",
        Phase::Active,
        "request",
        c.add(&[
            c.vec(1, manifest)?,
            c.json(manifest)?,
            selection_request as u64,
            path,
        ])?,
    );
    a.push(
        "transient.result_build_encode_and_page",
        Phase::Active,
        "result",
        c.add(&[c.vec(1, result)?, c.json(result)?, c.vec(1, page)?])?,
    );
    a.push(
        "transient.selection_documents_and_rosters",
        Phase::Active,
        "selection",
        c.add(&[
            approval_draft as u64,
            approval_documents as u64,
            c.mul(2 * ROSTER_LIMIT, approval_document as u64)?,
            c.mul(2, c.vec(1, manifest)?)?,
            c.mul(2, c.json(manifest)?)?,
        ])?,
    );
    a.push(
        "transient.source_callback_frame_and_typed_payload",
        Phase::Active,
        "callback",
        c.add(&[c.vec(1, callback)?, c.json(callback)?, c.vec(1, page)?])?,
    );
    a.push(
        "transient.w_exact_manifest_assembly",
        Phase::Active,
        "callback",
        // Exact source bytes are accumulated while the typed Evidence reply
        // remains live. String::from_utf8 reuses this single Vec allocation;
        // no second semantic manifest graph is constructed.
        c.vec(1, manifest)?,
    );
    a.push(
        "transient.seal_documents_and_preparation",
        Phase::Active,
        "seal",
        c.add(&[
            c.vec(1, manifest)?,
            c.json(manifest)?,
            c.vec(1, review)?,
            c.json(review)?,
            c.mul(ROSTER_LIMIT, approval_document as u64)?,
            path,
        ])?,
    );
    a.finish()
}

#[derive(Clone, Copy)]
pub(crate) struct Requirement {
    bytes: u64,
}

impl Requirement {
    pub(crate) fn from_config(config: &Config, control_slots: usize) -> Result<Self> {
        Ok(Self {
            bytes: report(config, control_slots)?.required,
        })
    }

    pub(crate) fn bytes(self) -> u64 {
        self.bytes
    }
}

pub(crate) struct Admission {
    held: Option<(ByteReservation, ByteReservation)>,
    checked_drained: AtomicBool,
}

impl Admission {
    pub(crate) fn arm(&self) {
        self.checked_drained.store(false, Ordering::Release);
    }
    pub(crate) fn checked_drained(&self) {
        self.checked_drained.store(true, Ordering::Release);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.checked_drained.load(Ordering::Acquire)
            && let Some(held) = self.held.take()
        {
            // Uncertain W/S/F reconciliation can retain every represented
            // graph. Keep both physical grants charged so replacement
            // admission fails closed.
            std::mem::forget(held);
        }
    }
}

pub(crate) struct Allocation {
    metadata: Admission,
    source_payloads: ByteBudget,
}

impl Allocation {
    pub(crate) fn from_subgrant(
        requirement: Requirement,
        reservation: ByteReservation,
        source_payloads: ByteBudget,
    ) -> Result<Self> {
        ensure!(
            reservation.bytes() == requirement.bytes,
            "Workbench metadata subgrant differs from checked requirement"
        );
        ensure!(
            !reservation.same_pool(&source_payloads),
            "Workbench metadata and Source payload pools must be distinct"
        );
        let source_required = source_requirement()?;
        let source_held = source_payloads
            .reserve_exact(source_required)
            .map_err(anyhow::Error::new)
            .context("Workbench Source high-water admission")?;
        Ok(Self {
            metadata: Admission {
                held: Some((reservation, source_held)),
                checked_drained: AtomicBool::new(true),
            },
            // Dynamic Source reservations use a private counter exactly equal
            // to the retained parent grant, so later phases cannot race another
            // user of the shared caller pool.
            source_payloads: ByteBudget::new(source_required)?,
        })
    }

    pub(crate) fn into_parts(self) -> (Admission, ByteBudget) {
        (self.metadata, self.source_payloads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            worker_executable: "/fixture/worker".into(),
            cache_root: None,
            original_roots: Vec::new(),
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        }
    }

    #[test]
    fn complete_report_names_every_owner_and_preserves_configured_queue() -> Result<()> {
        let config = config();
        let default_report = report(&config, 16)?;
        for name in [
            "g.dispatcher.fixed_owner_and_shared_layouts",
            "g.dispatcher.queued_and_active_typed_requests",
            "g.dispatcher.completion_permits_channels_and_cancellation",
            "transient.managed_document_callback_roots",
            "g.source_parent_and_private_budget_counters",
            "g.owner_generation_router_reader_custody",
            "g.capability_pending_sessions_and_receipts",
            "w.control_status_worker_and_bridge",
            "transient.w_progress_control_installation",
            "w.upload_single_staged_input",
            "w.cached_result_and_review",
            "f.owner_root",
            "f.capture_process_and_manifest",
            "f.evidence_manifest_source_roster",
            "f.original_candidate_and_encoded_result",
            "f.seal_paths_uploads_and_digests",
            "transient.f_evidence_roster_construction",
            "transient.request_decode_and_typed_action",
            "transient.result_build_encode_and_page",
            "transient.selection_documents_and_rosters",
            "transient.source_callback_frame_and_typed_payload",
            "transient.w_exact_manifest_assembly",
            "transient.seal_documents_and_preparation",
        ] {
            assert!(
                default_report
                    .contributions
                    .iter()
                    .any(|value| value.name == name),
                "missing {name}"
            );
        }
        assert_eq!(
            default_report.retained,
            default_report
                .contributions
                .iter()
                .filter(|value| value.phase == Phase::Retained)
                .map(|value| value.bytes)
                .sum::<u64>()
        );
        assert_eq!(
            default_report.active,
            default_report
                .contributions
                .iter()
                .filter(|value| value.phase == Phase::Active)
                .map(|value| value.bytes)
                .sum::<u64>()
        );
        assert!(
            dispatcher_layouts()
                .into_iter()
                .all(|(size, align)| size > 0 && align.is_power_of_two())
        );
        let queued = default_report
            .contributions
            .iter()
            .find(|value| value.name == "g.dispatcher.queued_and_active_typed_requests")
            .unwrap()
            .bytes;
        let mut wider = config.clone();
        wider.limits.queued += 1;
        let wider = report(&wider, 16)?;
        let wider_queued = wider
            .contributions
            .iter()
            .find(|value| value.name == "g.dispatcher.queued_and_active_typed_requests")
            .unwrap()
            .bytes;
        assert!(wider_queued > queued);
        assert_eq!(
            default_report.required,
            default_report.retained + default_report.active
        );
        assert!(SOURCE_PAYLOAD_POOL_CONTRACT.starts_with("distinct"));
        Ok(())
    }

    #[test]
    fn capability_ledger_funds_fixed_roots_and_bounded_receipt_roster() -> Result<()> {
        let report = report(&config(), 16)?;
        let actual = report
            .contributions
            .iter()
            .find(|value| value.name == "g.capability_pending_sessions_and_receipts")
            .context("capability custody contribution")?;
        let c = Checked;
        let managed = crate::application::lightroom_managed::metadata_layouts();
        let limits = super::super::lightroom::Limits::metadata_maximum();
        let path = c.native_path(limits.native_path_units as u64)?;
        let expected = c.add(&[
            managed.capability_custody as u64,
            c.mul(3, path)?,
            c.mul(3, c.string(IDENTITY_BYTES)?)?,
            c.mul(4, c.string(DIGEST_BYTES)?)?,
            c.vec(
                managed.receipt as u64,
                crate::application::lightroom_managed::CAPABILITY_RECEIPTS as u64,
            )?,
            c.mul(
                crate::application::lightroom_managed::CAPABILITY_RECEIPTS as u64 + 1,
                c.string(IDENTITY_BYTES)?,
            )?,
        ])?;
        assert_eq!(actual.phase, Phase::Retained);
        assert_eq!(actual.bytes, expected);
        Ok(())
    }

    #[test]
    fn subgrant_is_exact_once_and_source_pool_is_separate() -> Result<()> {
        let requirement = Requirement::from_config(&config(), 16)?;
        let source_required = source_requirement()?;
        let metadata = ByteBudget::new(requirement.bytes())?;
        let source = ByteBudget::new(source_required)?;
        let mut process = metadata.reserve_exact(requirement.bytes())?;
        let grant = process.split_exact(requirement.bytes())?;
        assert_eq!(metadata.used(), requirement.bytes());
        let allocation = Allocation::from_subgrant(requirement, grant, source.clone())?;
        assert_eq!(metadata.used(), requirement.bytes());
        assert_eq!(source.used(), source_required);
        let (admission, _) = allocation.into_parts();
        admission.checked_drained();
        drop(admission);
        assert_eq!(metadata.used(), 0);
        assert_eq!(source.used(), 0);

        let metadata = ByteBudget::new(requirement.bytes())?;
        let same = metadata.reserve_exact(requirement.bytes())?;
        assert!(Allocation::from_subgrant(requirement, same, metadata.clone()).is_err());
        let short = ByteBudget::new(requirement.bytes() - 1)?;
        let short = short.reserve_exact(requirement.bytes() - 1)?;
        assert!(Allocation::from_subgrant(requirement, short, ByteBudget::new(1)?).is_err());

        let metadata = ByteBudget::new(requirement.bytes())?;
        let grant = metadata.reserve_exact(requirement.bytes())?;
        let source = ByteBudget::new(source_required - 1)?;
        assert!(Allocation::from_subgrant(requirement, grant, source).is_err());
        Ok(())
    }

    #[test]
    fn unstarted_allocation_releases_both_grants() -> Result<()> {
        let requirement = Requirement::from_config(&config(), 16)?;
        let metadata = ByteBudget::new(requirement.bytes())?;
        let source = ByteBudget::new(source_requirement()?)?;
        let allocation = Allocation::from_subgrant(
            requirement,
            metadata.reserve_exact(requirement.bytes())?,
            source.clone(),
        )?;
        drop(allocation);
        assert_eq!(metadata.used(), 0);
        assert_eq!(source.used(), 0);
        Ok(())
    }
    #[test]
    fn unverified_drain_retains_both_physical_grants() -> Result<()> {
        let requirement = Requirement::from_config(&config(), 16)?;
        let source_required = source_requirement()?;
        let metadata = ByteBudget::new(requirement.bytes())?;
        let source = ByteBudget::new(source_required)?;
        let grant = metadata.reserve_exact(requirement.bytes())?;
        let allocation = Allocation::from_subgrant(requirement, grant, source.clone())?;
        let (admission, _) = allocation.into_parts();
        admission.arm();
        drop(admission);
        assert_eq!(metadata.used(), requirement.bytes());
        assert_eq!(source.used(), source_required);
        assert!(metadata.reserve_exact(1).is_err());
        assert!(source.reserve_exact(1).is_err());
        Ok(())
    }

    #[test]
    fn actual_failed_drain_retains_exact_pools_until_w_s_f_reconcile() -> Result<()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir()?;
        let config = Config {
            worker_executable: std::env::current_exe()?,
            cache_root: None,
            original_roots: Vec::new(),
            preview_policy: Default::default(),
            preview_limits: Default::default(),
            limits: Default::default(),
            import_checkpoint: None,
        };
        let requirement =
            Requirement::from_config(&config, crate::application::desktop::CONTROL_SLOTS)?;
        let source_required = source_requirement()?;
        let metadata = ByteBudget::new(requirement.bytes())?;
        let source = ByteBudget::new(source_required)?;
        let allocation = Allocation::from_subgrant(
            requirement,
            metadata.reserve_exact(requirement.bytes())?,
            source.clone(),
        )?;
        let root = std::fs::canonicalize(temp.path())?;
        let filesystem =
            std::sync::Arc::new(crate::filesystem_worker::client::migration_fixture(&root)?);
        filesystem.wait_ready(std::time::Duration::from_secs(20))?;
        let owner = crate::application::lightroom_managed::Owner::start(
            &filesystem,
            &std::env::current_exe()?,
            allocation,
        )?;
        let generation = crate::application::lightroom_managed::Generation::start_fixture(
            &owner,
            &std::env::current_exe()?,
        )?;

        assert_eq!(metadata.used(), requirement.bytes());
        assert_eq!(source.used(), source_required);
        generation.interrupt()?;
        let failure = owner.drain_checked().unwrap_err();
        assert!(format!("{failure:#}").contains("W is checked-reaped"));
        assert_eq!(metadata.used(), requirement.bytes());
        assert_eq!(source.used(), source_required);

        let shutdown = generation.shutdown_checked().unwrap_err();
        assert!(format!("{shutdown:#}").contains("interrupted Workbench"));
        assert!(generation.pid().is_none());
        owner.drain_checked()?;
        assert_eq!(metadata.used(), requirement.bytes());
        assert_eq!(source.used(), source_required);
        drop(generation);
        drop(owner);
        assert_eq!(metadata.used(), 0);
        assert_eq!(source.used(), 0);
        filesystem.try_shutdown()?;
        Ok(())
    }
}
