//! Production Lightroom migration child. The main thread alone parses approved
//! documents, opens destination SQL and executes migration work. A separate
//! listener accepts only cancellation, grants and Source relay control.
use super::{
    authority::ApprovedAdmitted,
    identity::{Audit, FileKey},
    input::{self, AdmittedPart, DocumentSet, PartDescriptor},
    lease::{DestinationLease, DestinationPin, DestinationReview},
    memory::{MemoryBudget, Reservation},
    protocol::{
        self, ChildFrame, Controls, Grants, Guard, InputRole, MemoryGrants, ParentFrame, Publish,
        WriteKind,
    },
    source_reader::{CommitHealth, RemoteArtifacts, SqlReader, relay::client::Client},
};
use crate::{
    application::U64,
    catalog_migration::{
        artifacts::ArtifactLimits,
        current_repair, keyword_repair,
        lightroom_executor::{self, Output, WorkLimit},
        supplements,
    },
    catalog_writer::Writers,
    lightroom::migration_source::{InputSeal, MigrationRead, ReadLimits},
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
    time::{Duration, Instant},
};

struct SharedInput<R>(Arc<Mutex<R>>);
impl<R> Clone for SharedInput<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<R: Read> Read for SharedInput<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("migration input reader poisoned"))?
            .read(buffer)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub protocol: u8,
    pub build: String,
    pub target_token: String,
    pub destination: NativePath,
    pub expected_destination: Option<DestinationPin>,
    pub protected: Vec<FileKey>,
    pub parts: Vec<PartDescriptor>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Run {
        approval_blake3: String,
        max_steps: U64,
        max_seconds: U64,
        source_open_ms: U64,
        artifact_open_ms: U64,
        max_artifact_bytes: U64,
    },
    Status {
        run: String,
    },
    PrepareSupplements,
    RepairCurrent {
        max_steps: U64,
        max_seconds: U64,
        source_open_ms: U64,
    },
    RepairStatus {
        repair: String,
    },
    RepairKeywords {
        max_steps: U64,
        max_seconds: U64,
        source_open_ms: U64,
    },
    KeywordRepairStatus {
        repair: String,
    },
}

impl Operation {
    fn documents(&self, parts: &[PartDescriptor]) -> Result<()> {
        let set = match self {
            Self::Run { .. }
                if parts
                    .last()
                    .is_some_and(|p| p.role == InputRole::ExecutionAuthorization) =>
            {
                DocumentSet::RunWithAuthorization
            }
            Self::Run { .. } => DocumentSet::Run,
            Self::RepairCurrent { .. } | Self::RepairKeywords { .. } => DocumentSet::Repair,
            Self::PrepareSupplements => DocumentSet::PrepareSupplements,
            Self::Status { .. } | Self::RepairStatus { .. } | Self::KeywordRepairStatus { .. } => {
                DocumentSet::None
            }
        };
        set.validate(parts)?;
        Ok(())
    }
    pub(crate) fn result_maximum(&self) -> Result<usize> {
        fn expanded(bytes: usize) -> Result<usize> {
            bytes
                .checked_mul(6)
                .context("migration result JSON expansion overflow")
        }
        match self {
            Self::PrepareSupplements => protocol::result::prepared_supplements_bound(1024),
            Self::Status { .. } | Self::RepairStatus { .. } | Self::KeywordRepairStatus { .. } => {
                expanded(lightroom_executor::DOCUMENT_BYTES as usize)
            }
            // Progress documents are stored through the existing 8 MiB bounded
            // encoders. A decoded source byte can require at most six JSON bytes.
            // Run may additionally return one accepted decision reason (the
            // largest producer bound is 16 KiB). The literal below includes the
            // largest operation wrapper and full-width u64/f64 scalar spellings.
            Self::Run { .. } | Self::RepairCurrent { .. } | Self::RepairKeywords { .. } => {
                const DECISION_BYTES: usize = 16 * 1024;
                const WRAPPER: usize = br#"{"protocol":1,"status":"needs_decision","progress":,"steps":18446744073709551615,"elapsed_seconds":-1.7976931348623157e308,"needs_decision":,"adobe_rendering_equivalent":false,"native_collection_order_equivalent":false}"#.len();
                expanded(8 * 1024 * 1024)?
                    .checked_add(expanded(DECISION_BYTES)?)
                    .and_then(|n| n.checked_add(WRAPPER))
                    .context("migration result bound overflow")
            }
        }
    }
    fn source_open_ms(&self) -> Option<u64> {
        match self {
            Self::Run { source_open_ms, .. }
            | Self::RepairCurrent { source_open_ms, .. }
            | Self::RepairKeywords { source_open_ms, .. } => Some(source_open_ms.0),
            _ => None,
        }
    }
}

impl Envelope {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.protocol == 1, "unsupported migration worker protocol");
        ensure!(
            self.build == build_identity(),
            "migration worker build mismatch"
        );
        ensure!(
            self.target_token.len() == 64
                && self.target_token.bytes().all(|b| b.is_ascii_hexdigit()),
            "migration target token bounds"
        );
        super::authority::local_destination(&self.destination)?;
        ensure!(
            self.protected.len() <= 4096,
            "migration protected object roster bound"
        );
        self.operation.documents(&self.parts)
    }
}

struct Parts<G> {
    seal: Option<AdmittedPart<G>>,
    approval: Option<AdmittedPart<G>>,
    policy: Option<AdmittedPart<G>>,
    repair: Option<AdmittedPart<G>>,
    supplements: Option<AdmittedPart<G>>,
    authorization: Option<AdmittedPart<G>>,
}
// Field order is destruction order: the Source process and its CommitHealth
// dependencies retire before the destination lock can be released.
struct ExecutionOwners {
    source: Option<SqlReader>,
    lease: Arc<DestinationLease>,
}

enum PreparedOperation<'a> {
    Run(ApprovedAdmitted<'a>),
    Status,
    Supplements(Vec<supplements::Request>),
    Current(InputSeal, current_repair::Request),
    CurrentStatus,
    Keywords(InputSeal, keyword_repair::Request),
    KeywordStatus,
}
impl<G> Default for Parts<G> {
    fn default() -> Self {
        Self {
            seal: None,
            approval: None,
            policy: None,
            repair: None,
            supplements: None,
            authorization: None,
        }
    }
}
impl<G> Parts<G> {
    fn insert(&mut self, role: InputRole, part: AdmittedPart<G>) -> Result<()> {
        let slot = match role {
            InputRole::Seal => &mut self.seal,
            InputRole::Approval => &mut self.approval,
            InputRole::Policy => &mut self.policy,
            InputRole::RepairRequest => &mut self.repair,
            InputRole::SupplementRequests => &mut self.supplements,
            InputRole::ExecutionAuthorization => &mut self.authorization,
            InputRole::Operation => anyhow::bail!("operation cannot repeat as a document part"),
        };
        ensure!(
            slot.replace(part).is_none(),
            "duplicate migration input role"
        );
        Ok(())
    }
    fn text<'a>(slot: &'a Option<AdmittedPart<G>>, name: &'static str) -> Result<&'a str> {
        Ok(slot
            .as_ref()
            .with_context(|| format!("{name} document required"))?
            .text())
    }
    fn bytes(slot: &Option<AdmittedPart<G>>) -> usize {
        slot.as_ref().map_or(0, |part| part.text().len())
    }
}

fn operation_allocation(envelope: &Envelope, parts: &Parts<Reservation>) -> Result<usize> {
    use super::memory::core;
    match &envelope.operation {
        Operation::Run { .. } => core::worker_run_documents(
            Parts::bytes(&parts.seal),
            Parts::bytes(&parts.approval),
            Parts::bytes(&parts.policy),
            Parts::bytes(&parts.authorization),
        ),
        Operation::RepairCurrent { .. } | Operation::RepairKeywords { .. } => {
            core::worker_repair_documents(Parts::bytes(&parts.seal), Parts::bytes(&parts.repair))
        }
        Operation::PrepareSupplements => {
            core::worker_supplement_documents(Parts::bytes(&parts.supplements))
        }
        Operation::Status { .. }
        | Operation::RepairStatus { .. }
        | Operation::KeywordRepairStatus { .. } => core::worker_status(),
    }
}

fn native_capacity(path: &NativePath) -> Result<usize> {
    match path {
        NativePath::UnixBytes(value) => Ok(value.capacity()),
        NativePath::WindowsWide(value) => value
            .capacity()
            .checked_mul(std::mem::size_of::<u16>())
            .context("supplement native path capacity overflow"),
    }
}

fn supplement_requests_retained(requests: &Vec<supplements::Request>) -> Result<usize> {
    use super::memory::layout::{add, mul};
    requests.iter().try_fold(
        mul(
            requests.capacity(),
            std::mem::size_of::<supplements::Request>(),
        )?,
        |total, request| {
            add(
                total,
                add(
                    add(
                        native_capacity(&request.proof_root)?,
                        native_capacity(&request.inspection_relative)?,
                    )?,
                    add(
                        request.inspection_blake3.capacity(),
                        add(
                            request.capture_revision.capacity(),
                            add(
                                request.source_id.capacity(),
                                request.source_revision.blake3.capacity(),
                            )?,
                        )?,
                    )?,
                )?,
            )
        },
    )
}

fn execution_allocation(prepared: &PreparedOperation<'_>) -> Result<Option<usize>> {
    use super::memory::{core, layout::add};
    match prepared {
        PreparedOperation::Supplements(requests) => Ok(Some(add(
            supplement_requests_retained(requests)?,
            core::worker_supplement_execution(requests.len())?,
        )?)),
        _ => Ok(None),
    }
}

fn prepare<'a>(
    envelope: &Envelope,
    parts: &'a Parts<Reservation>,
) -> Result<PreparedOperation<'a>> {
    Ok(match &envelope.operation {
        Operation::Run {
            approval_blake3, ..
        } => PreparedOperation::Run(ApprovedAdmitted::parse(
            Parts::text(&parts.seal, "seal")?,
            Parts::text(&parts.approval, "approval")?,
            Parts::text(&parts.policy, "policy")?,
            parts.authorization.as_ref().map(AdmittedPart::text),
            envelope.destination.clone(),
            approval_blake3,
        )?),
        Operation::Status { .. } => PreparedOperation::Status,
        Operation::PrepareSupplements => PreparedOperation::Supplements(serde_json::from_str(
            Parts::text(&parts.supplements, "supplement requests")?,
        )?),
        Operation::RepairCurrent { .. } => {
            let seal: InputSeal = serde_json::from_str(Parts::text(&parts.seal, "seal")?)?;
            let approval = Parts::text(&parts.approval, "approval")?;
            ensure!(
                blake3::hash(approval.as_bytes()).to_hex().as_str()
                    == seal.approval.document_blake3,
                "authorization bytes differ from seal"
            );
            PreparedOperation::Current(
                seal,
                serde_json::from_str(Parts::text(&parts.repair, "repair request")?)?,
            )
        }
        Operation::RepairStatus { .. } => PreparedOperation::CurrentStatus,
        Operation::RepairKeywords { .. } => {
            let seal: InputSeal = serde_json::from_str(Parts::text(&parts.seal, "seal")?)?;
            let approval = Parts::text(&parts.approval, "approval")?;
            ensure!(
                blake3::hash(approval.as_bytes()).to_hex().as_str()
                    == seal.approval.document_blake3,
                "authorization bytes differ from seal"
            );
            PreparedOperation::Keywords(
                seal,
                serde_json::from_str(Parts::text(&parts.repair, "repair request")?)?,
            )
        }
        Operation::KeywordRepairStatus { .. } => PreparedOperation::KeywordStatus,
    })
}

/// This identity is checked by LM and both Source roles before any authority or
/// filesystem access. Keep the list explicit so a behavior-bearing source edit
/// necessarily changes the managed process handshake.
pub(crate) fn build_identity() -> &'static str {
    static VALUE: OnceLock<String> = OnceLock::new();
    VALUE.get_or_init(|| {
        let mut h = blake3::Hasher::new();
        for bytes in [
            include_bytes!("../lightroom_migration_worker.rs").as_slice(),
            include_bytes!("worker.rs").as_slice(),
            include_bytes!("supervisor.rs").as_slice(),
            include_bytes!("supervisor/pending.rs").as_slice(),
            include_bytes!("../catalog_migration/lightroom_executor.rs").as_slice(),
            include_bytes!("../catalog_migration.rs").as_slice(),
            include_bytes!("../bin/lightroom_migrate.rs").as_slice(),
            include_bytes!("authority.rs").as_slice(),
            include_bytes!("identity.rs").as_slice(),
            include_bytes!("input.rs").as_slice(),
            include_bytes!("lease.rs").as_slice(),
            include_bytes!("memory.rs").as_slice(),
            include_bytes!("memory/channels.rs").as_slice(),
            include_bytes!("memory/core.rs").as_slice(),
            include_bytes!("memory/layout.rs").as_slice(),
            include_bytes!("memory/requested.rs").as_slice(),
            include_bytes!("memory/transport.rs").as_slice(),
            include_bytes!("process.rs").as_slice(),
            include_bytes!("protocol.rs").as_slice(),
            include_bytes!("protocol/result.rs").as_slice(),
            include_bytes!("source_reader.rs").as_slice(),
            include_bytes!("source_reader/transport.rs").as_slice(),
            include_bytes!("source_reader/authority_json.rs").as_slice(),
            include_bytes!("source_reader/wire.rs").as_slice(),
            include_bytes!("source_reader/owner.rs").as_slice(),
            include_bytes!("source_reader/proxy.rs").as_slice(),
            include_bytes!("source_reader/artifact_factory.rs").as_slice(),
            include_bytes!("source_reader/commit.rs").as_slice(),
            include_bytes!("source_reader/relay.rs").as_slice(),
            include_bytes!("source_reader/relay/broker.rs").as_slice(),
            include_bytes!("source_reader/relay/client.rs").as_slice(),
            include_bytes!("source_reader/relay/server.rs").as_slice(),
            include_bytes!("../catalog_writer.rs").as_slice(),
            include_bytes!("../catalog_session.rs").as_slice(),
            include_bytes!("../catalog_migration/importer.rs").as_slice(),
            include_bytes!("../catalog_migration/import_artifacts.rs").as_slice(),
            include_bytes!("../catalog_migration/evidence.rs").as_slice(),
            include_bytes!("../catalog_migration/current_repair.rs").as_slice(),
            include_bytes!("../catalog_migration/keyword_repair.rs").as_slice(),
            include_bytes!("../catalog_migration/supplements.rs").as_slice(),
            include_bytes!("../catalog_migration/file_metadata.rs").as_slice(),
            include_bytes!("../catalog_migration/metadata.rs").as_slice(),
            include_bytes!("../catalog_migration/organization.rs").as_slice(),
            include_bytes!("../catalog_migration/organization/candidates.rs").as_slice(),
            include_bytes!("../catalog_migration/organization_walk.rs").as_slice(),
            include_bytes!("../catalog_migration/history.rs").as_slice(),
            include_bytes!("../catalog_migration/artifacts.rs").as_slice(),
            include_bytes!("../catalog_migration/artifacts/descriptor_json.rs").as_slice(),
            include_bytes!("../catalog_migration/artifacts/preparation.rs").as_slice(),
            include_bytes!("../catalog_migration/images.rs").as_slice(),
            include_bytes!("../catalog_migration/lookup.rs").as_slice(),
            include_bytes!("../catalog_migration/originals.rs").as_slice(),
            include_bytes!("../catalog_migration/reconciliation.rs").as_slice(),
            include_bytes!("../catalog_migration/retention.rs").as_slice(),
            include_bytes!("../catalog_migration/repair_memory.rs").as_slice(),
            include_bytes!("../catalog_migration/saved.rs").as_slice(),
            include_bytes!("../catalog_migration/walk.rs").as_slice(),
            include_bytes!("../lightroom/migration_source.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/access.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/buffered_json.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/manifest_json.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/manifest_json/admission.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/record_json.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/reader.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/seal_json.rs").as_slice(),
            include_bytes!("../lightroom/migration_source/supplement_json.rs").as_slice(),
            include_bytes!("../catalog_metadata.rs").as_slice(),
            include_bytes!("../xmp_packets.rs").as_slice(),
            include_bytes!("../xmp.rs").as_slice(),
            include_bytes!("../xmp_rdf.rs").as_slice(),
            include_bytes!("../xmp/semantics_json.rs").as_slice(),
            include_bytes!("../../src/main.rs").as_slice(),
            include_bytes!("../../desktop/src-tauri/src/main.rs").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/admitted_string.rs").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/lib.rs").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/xmp_meta.rs").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/ffi.rs").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/ffi.cpp").as_slice(),
            include_bytes!("../../vendor/xmp_toolkit/src/ffi/bounded_string.rs").as_slice(),
            include_bytes!("../../Cargo.toml").as_slice(),
            include_bytes!("../../Cargo.lock").as_slice(),
        ] {
            h.update(bytes);
        }
        h.finalize().to_hex().to_string()
    })
}

fn publish_result(
    output: &dyn Publish,
    controls: &Controls,
    guard: &Guard,
    value: &Output,
    maximum: usize,
) -> Result<()> {
    let measured = protocol::result::measure(value, maximum)?;
    let grant = controls.request_result(&measured, maximum, output)?;
    protocol::result::publish(value, grant, guard, output)
}

/// Audit the original spelling, including its nearest existing ancestor when
/// the requested directory does not exist yet. Canonical custody comparisons
/// do not authorize following a link during managed destination creation.
fn check_destination_path(destination: &std::path::Path, audit: &Audit) -> Result<()> {
    let checked = (|| {
        audit.check()?;
        let mut ancestor = destination;
        let metadata = loop {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) => break metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().context("destination has no ancestor")?;
                }
                Err(error) => return Err(error.into()),
            }
        };
        crate::lightroom::source::reject_links(ancestor)?;
        ensure!(
            metadata.is_dir(),
            "destination ancestor must be a directory"
        );
        Ok(())
    })();
    if checked.is_err() {
        audit.poison();
    }
    checked
}

fn review_destination(
    envelope: &Envelope,
    audit: &Audit,
) -> Result<Option<Arc<DestinationReview>>> {
    let destination = super::authority::local_destination(&envelope.destination)?;
    check_destination_path(&destination, audit)?;
    let observed = if destination.join("catalog.sqlite3").is_file() {
        Some(Arc::new(DestinationReview::existing(
            &envelope.destination,
            None,
            audit,
        )?))
    } else {
        None
    };
    ensure!(
        observed.is_some() || envelope.expected_destination.is_none(),
        "pinned destination database disappeared"
    );
    if let (Some(review), Some(expected)) = (&observed, &envelope.expected_destination) {
        ensure!(
            review.pin.root == expected.root
                && review.pin.root_key == expected.root_key
                && review.pin.database_key == expected.database_key
                && (review.pin.schema == expected.schema
                    || (expected.schema.0 < crate::CURRENT_SCHEMA_VERSION
                        && review.pin.schema.0 == crate::CURRENT_SCHEMA_VERSION)),
            "reviewed destination identity changed before bootstrap"
        );
    }
    Ok(observed)
}

fn prepare_destination(
    envelope: &Envelope,
    audit: &Audit,
    observed: Option<Arc<DestinationReview>>,
    controls: Arc<Controls>,
    output: Arc<dyn Publish>,
) -> Result<Option<DestinationPin>> {
    let destination = super::authority::local_destination(&envelope.destination)?;
    check_destination_path(&destination, audit)?;
    if let Some(review) = &observed {
        review.verify()?;
    }
    let database_needed = observed
        .as_ref()
        .is_none_or(|review| review.pin.schema.0 != crate::CURRENT_SCHEMA_VERSION)
        || envelope
            .expected_destination
            .as_ref()
            .is_some_and(|pin| pin.schema.0 != crate::CURRENT_SCHEMA_VERSION);
    let mut checked_pin = observed.as_ref().map(|review| review.pin.clone());
    if let Some(pin) = &mut checked_pin {
        pin.schema = crate::application::I64(crate::CURRENT_SCHEMA_VERSION);
    }
    let lock_path = destination.join(".lightroom-import.lock");
    let lock_needed = !lock_path.is_file();
    if !database_needed && !lock_needed {
        return Ok(checked_pin);
    }
    let checked = audit.clone();
    let checked_destination = destination.clone();
    let grant_review = observed.clone();
    let writers = Writers::with_external(Arc::new(Grants {
        controls,
        output,
        write: WriteKind::Bootstrap,
        target_token: envelope.target_token.clone(),
        lock: None,
        verify: Arc::new(move || {
            check_destination_path(&checked_destination, &checked)?;
            if let Some(review) = &grant_review {
                review.verify()?;
            }
            Ok(())
        }),
    }));
    // The parent performs the reviewed create/upgrade before replying Grant.
    // Reviewed reads above carry the existing object pin; write permission
    // starts only when that acknowledgement returns.
    let permit = writers.enter(crate::catalog_writer::Priority::Background)?;
    ensure!(
        destination.join("catalog.sqlite3").is_file(),
        "bootstrap grant did not publish destination catalog"
    );
    if lock_needed {
        drop(audit.create_import_lock(&lock_path)?);
    }
    drop(permit);
    ensure!(
        lock_path.is_file(),
        "bootstrap grant did not publish import lock"
    );
    if let Some(review) = observed {
        review.verify()?;
    }
    Ok(checked_pin)
}

fn validate_before_source(
    envelope: &Envelope,
    prepared: &PreparedOperation<'_>,
    destination: &std::path::Path,
) -> Result<()> {
    match (&envelope.operation, prepared) {
        (
            Operation::Run {
                max_steps,
                max_seconds,
                artifact_open_ms,
                max_artifact_bytes,
                ..
            },
            PreparedOperation::Run(approved),
        ) => {
            WorkLimit {
                steps: max_steps.0,
                seconds: max_seconds.0,
            }
            .validate()?;
            ArtifactLimits {
                maximum_bytes: max_artifact_bytes.0,
                open_deadline_ms: artifact_open_ms.0,
                chunk_deadline_ms: 120_000,
                chunk_bytes: 1024 * 1024,
            }
            .validate()?;
            let database = approved.seal().database.to_path()?;
            lightroom_executor::disjoint(
                destination,
                database.parent().context("inspection has no parent")?,
            )?;
            for artifact in &approved.policy().artifacts {
                lightroom_executor::disjoint(destination, &artifact.mapping.root.to_path()?)?;
            }
        }
        (
            Operation::RepairCurrent {
                max_steps,
                max_seconds,
                ..
            }
            | Operation::RepairKeywords {
                max_steps,
                max_seconds,
                ..
            },
            PreparedOperation::Current(seal, _) | PreparedOperation::Keywords(seal, _),
        ) => {
            WorkLimit {
                steps: max_steps.0,
                seconds: max_seconds.0,
            }
            .validate()?;
            ensure!(
                destination.join("catalog.sqlite3").is_file(),
                "repair requires an existing catalog"
            );
            let database = seal.database.to_path()?;
            lightroom_executor::disjoint(
                destination,
                database.parent().context("inspection has no parent")?,
            )?;
        }
        (Operation::PrepareSupplements, PreparedOperation::Supplements(requests)) => {
            ensure!(
                !requests.is_empty() && requests.len() <= 1024,
                "supplement request roster bound"
            );
            for request in requests {
                supplements::validate_request(request)?;
                lightroom_executor::disjoint(destination, &request.proof_root.to_path()?)?;
            }
        }
        (Operation::Status { .. }, PreparedOperation::Status)
        | (Operation::RepairStatus { .. }, PreparedOperation::CurrentStatus)
        | (Operation::KeywordRepairStatus { .. }, PreparedOperation::KeywordStatus) => {}
        _ => anyhow::bail!("migration operation/prepared input roster differs"),
    }
    Ok(())
}

fn preflight_destination(
    prepared: &PreparedOperation<'_>,
    input: Option<&str>,
    observed: Option<&Arc<DestinationReview>>,
) -> Result<()> {
    let _phase = crate::catalog_migration::repair_memory::phase();
    match prepared {
        PreparedOperation::Current(_, request) => {
            let review = observed.context("repair destination review absent")?;
            let db = review.read()?;
            lightroom_executor::preflight_current_database(
                &db,
                input.context("repair Source absent")?,
                request,
            )?;
            review.verify()?;
        }
        PreparedOperation::Keywords(_, request) => {
            let review = observed.context("repair destination review absent")?;
            let db = review.read()?;
            lightroom_executor::preflight_keyword_database(
                &db,
                input.context("repair Source absent")?,
                request,
            )?;
            review.verify()?;
        }
        _ => {}
    }
    Ok(())
}

fn execute(
    envelope: &Envelope,
    prepared: &PreparedOperation<'_>,
    guard: &Guard,
    controls: Arc<Controls>,
    output: Arc<dyn Publish>,
    audit: &Audit,
    relay: Arc<Client>,
    cancel: Arc<AtomicBool>,
) -> Result<Output> {
    let destination = lightroom_executor::destination_path(&super::authority::local_destination(
        &envelope.destination,
    )?)?;
    validate_before_source(envelope, prepared, &destination)?;
    if matches!(
        envelope.operation,
        Operation::Status { .. }
            | Operation::RepairStatus { .. }
            | Operation::KeywordRepairStatus { .. }
    ) {
        let review = DestinationReview::existing(
            &envelope.destination,
            envelope.expected_destination.as_ref(),
            audit,
        )?;
        ensure!(
            review.pin.schema.0 == crate::CURRENT_SCHEMA_VERSION,
            "status requires a current LensWorks catalog; no schema migration was performed"
        );
        let db = review.read()?;
        return match &envelope.operation {
            Operation::Status { run } => lightroom_executor::run_status(&db, run),
            Operation::RepairStatus { repair } => {
                lightroom_executor::current_repair_status(&db, repair)
            }
            Operation::KeywordRepairStatus { repair } => {
                lightroom_executor::keyword_repair_status(&db, repair)
            }
            _ => unreachable!(),
        };
    }

    let source = if let Some(open_ms) = envelope.operation.source_open_ms() {
        ensure!(
            (1..=3_600_000).contains(&open_ms),
            "invalid source admission budget"
        );
        let seal = match prepared {
            PreparedOperation::Run(approved) => approved.seal().clone(),
            PreparedOperation::Current(seal, _) | PreparedOperation::Keywords(seal, _) => {
                seal.clone()
            }
            _ => anyhow::bail!("migration Source operation lacks a sealed input"),
        };
        let mut protected = Vec::new();
        if let Some(pin) = &envelope.expected_destination {
            protected.push(pin.root_key.clone());
            protected.push(pin.database_key.clone());
        }
        Some(SqlReader::open(
            relay.clone(),
            guard.clone(),
            "selected-sql".into(),
            seal,
            ReadLimits {
                open_deadline_ms: open_ms,
                ..ReadLimits::default()
            },
            protected,
            cancel.clone(),
        )?)
    } else {
        None
    };

    let observed = review_destination(envelope, audit)?;
    preflight_destination(
        prepared,
        source.as_ref().map(|source| source.binding_blake3()),
        observed.as_ref(),
    )?;
    let prepared_pin =
        prepare_destination(envelope, audit, observed, controls.clone(), output.clone())?;
    let review = DestinationReview::existing(&envelope.destination, prepared_pin.as_ref(), audit)?;
    let lease = Arc::new(DestinationLease::acquire(
        review,
        None,
        Instant::now() + Duration::from_secs(60),
    )?);
    let owners = ExecutionOwners { source, lease };
    output.publish(&ChildFrame::LockAcquired {
        guard: guard.clone(),
        lock: owners.lease.lock_key().clone(),
        destination: owners.lease.pin().clone(),
        target_token: envelope.target_token.clone(),
    })?;
    let verified = owners.lease.clone();
    let lease_key = owners.lease.lock_key().clone();
    let writers = Writers::with_external(Arc::new(Grants {
        controls: controls.clone(),
        output: output.clone(),
        write: WriteKind::Catalog,
        target_token: envelope.target_token.clone(),
        lock: Some(lease_key),
        verify: Arc::new(move || verified.verify()),
    }));

    let mut report = |phase: &str, completed: u64, total: Option<u64>| {
        output.publish(&ChildFrame::Progress {
            guard: guard.clone(),
            phase: phase.into(),
            completed: U64(completed),
            total: total.map(U64),
        })
    };
    let result = match (&envelope.operation, prepared, owners.source.as_ref()) {
        (Operation::PrepareSupplements, PreparedOperation::Supplements(requests), None) => {
            let mut catalog = owners.lease.open_current(writers)?;
            lightroom_executor::prepare_supplements_managed(
                &mut catalog,
                requests,
                cancel.as_ref(),
                &mut report,
            )
        }
        (
            operation @ Operation::Run {
                max_steps,
                max_seconds,
                artifact_open_ms,
                max_artifact_bytes,
                ..
            },
            PreparedOperation::Run(approved),
            Some(source),
        ) => {
            ensure!(
                approved.seal().binding_blake3()? == source.binding_blake3(),
                "approved source binding differs"
            );
            let health = CommitHealth::new(source.health());
            let mut catalog = owners
                .lease
                .open_current_with_sources(writers, health.clone())?;
            let factory = RemoteArtifacts {
                relay,
                guard: guard.clone(),
                protected: vec![
                    owners.lease.pin().root_key.clone(),
                    owners.lease.pin().database_key.clone(),
                    owners.lease.lock_key().clone(),
                ],
                cancel: cancel.clone(),
                health,
                next_epoch: 1,
            };
            let Operation::Run { .. } = operation else {
                unreachable!()
            };
            lightroom_executor::run_managed(
                &mut catalog,
                source,
                approved.approval_bytes(),
                approved.policy(),
                ArtifactLimits {
                    maximum_bytes: max_artifact_bytes.0,
                    open_deadline_ms: artifact_open_ms.0,
                    chunk_deadline_ms: 120_000,
                    chunk_bytes: 1024 * 1024,
                },
                WorkLimit {
                    steps: max_steps.0,
                    seconds: max_seconds.0,
                },
                &|| {
                    controls.check_active()?;
                    Ok(false)
                },
                &mut report,
                Box::new(factory),
            )
        }
        (
            Operation::RepairCurrent {
                max_steps,
                max_seconds,
                ..
            },
            PreparedOperation::Current(_, request),
            Some(source),
        ) => {
            let health = CommitHealth::new(source.health());
            let mut catalog = owners.lease.open_current_with_sources(writers, health)?;
            lightroom_executor::repair_current_managed(
                &mut catalog,
                source,
                request,
                WorkLimit {
                    steps: max_steps.0,
                    seconds: max_seconds.0,
                },
                &|| {
                    controls.check_active()?;
                    Ok(false)
                },
                &mut report,
            )
        }
        (
            Operation::RepairKeywords {
                max_steps,
                max_seconds,
                ..
            },
            PreparedOperation::Keywords(_, request),
            Some(source),
        ) => {
            let health = CommitHealth::new(source.health());
            let mut catalog = owners.lease.open_current_with_sources(writers, health)?;
            lightroom_executor::repair_keywords_managed(
                &mut catalog,
                source,
                request,
                WorkLimit {
                    steps: max_steps.0,
                    seconds: max_seconds.0,
                },
                &|| {
                    controls.check_active()?;
                    Ok(false)
                },
                &mut report,
            )
        }
        _ => anyhow::bail!("migration operation/source roster differs"),
    };
    // Catalogs, writer permits and CommitHealth owners in the selected arm are
    // gone. Retire Source while the destination lock is still retained, then
    // release the physical lock before the result is published.
    drop(owners);
    result
}

fn publish_failure(
    output: &dyn Publish,
    guard: Guard,
    audit: &Audit,
    error: anyhow::Error,
) -> Result<()> {
    let mut detail = format!("{error:#}");
    if detail.len() > 32 * 1024 {
        let mut end = 32 * 1024;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    output.publish(&ChildFrame::Failed {
        guard,
        detail,
        poisoned: audit.is_poisoned(),
    })
}

pub(crate) fn serve(
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
) -> Result<()> {
    let mut input = SharedInput(Arc::new(Mutex::new(input)));
    let received = input::receive(&mut input, Instant::now() + Duration::from_secs(60))?;
    let envelope: Envelope = serde_json::from_str(&received.text)?;
    ensure!(
        received.digest == blake3::hash(received.text.as_bytes()).to_hex().as_str(),
        "migration operation digest differs"
    );
    envelope.validate()?;
    let output: Arc<dyn Publish> = Arc::new(Mutex::new(output));
    let cancel = Arc::new(AtomicBool::new(false));
    let audit = Audit::new(cancel.clone(), envelope.protected.clone())?;
    let controls = Controls::new(
        received.guard.clone(),
        audit.clone(),
        Instant::now() + Duration::from_secs(86_460),
    )?;
    let mut startup_input = input.clone();
    let grants = Arc::new(MemoryGrants::with_startup(
        controls.clone(),
        output.clone(),
        move || {
            protocol::read_frame_optional::<ParentFrame>(&mut startup_input)?
                .context("migration input ended while awaiting startup memory grant")
        },
    ));
    let memory = MemoryBudget::from_parent(grants.clone());
    let parts = (|| -> Result<Parts<Reservation>> {
        let mut values = Parts::default();
        for expected in &envelope.parts {
            let part = input::receive_part_admitted(
                &mut input,
                &received.guard,
                expected,
                Instant::now() + Duration::from_secs(60),
                |bytes| {
                    let mut reservation = memory.reservation();
                    reservation.grow(bytes)?;
                    Ok(reservation)
                },
            )?;
            values.insert(expected.role, part)?;
        }
        grants.finish_startup()?;
        Ok(values)
    })();
    let parts = match parts {
        Ok(parts) => parts,
        Err(error) => return publish_failure(output.as_ref(), received.guard, &audit, error),
    };
    let listener = controls.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name("migration-control".into())
        .spawn(move || {
            if ready_tx.send(()).is_err() {
                listener.poison();
                return;
            }
            loop {
                match protocol::read_frame_optional::<ParentFrame>(&mut input) {
                    Ok(Some(frame)) => {
                        if listener.accept(frame).is_err() {
                            listener.poison();
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        listener.poison();
                        break;
                    }
                }
            }
        })?;
    ready_rx
        .recv()
        .context("migration control listener failed before startup handoff")?;
    output.publish(&ChildFrame::Admitted {
        guard: received.guard.clone(),
        request_blake3: received.digest,
        build: build_identity().into(),
    })?;
    // Raw part reservations are sibling owners. This operation reservation
    // covers typed/parser/execution graphs and remains live through result
    // measurement and publication; nested Source requests use the same parent
    // pool while both sets of owners are retained.
    #[cfg(all(test, unix))]
    let _adobe_observer =
        tests::unix::install_adobe_observer(output.clone(), received.guard.clone())?;
    let mut operation_memory = memory.reservation();
    let mut repair_memory = None;
    let attempted = (|| -> Result<()> {
        operation_memory.grow(operation_allocation(&envelope, &parts)?)?;
        let prepared = prepare(&envelope, &parts)?;
        if let Some(required) = execution_allocation(&prepared)? {
            operation_memory.ensure_at_least(required)?;
        }
        if matches!(
            prepared,
            PreparedOperation::Run(..)
                | PreparedOperation::Current(..)
                | PreparedOperation::Keywords(..)
        ) {
            repair_memory = Some(crate::catalog_migration::repair_memory::Operation::install(
                &memory,
            )?);
        }
        let controls_for_abort = controls.clone();
        let relay = Client::new(
            received.guard.clone(),
            output.clone(),
            Arc::new(move || controls_for_abort.poison()),
            memory.clone(),
        )?;
        controls.install_sources(relay.clone())?;
        let _scope = audit.install()?;
        let result = execute(
            &envelope,
            &prepared,
            &received.guard,
            controls.clone(),
            output.clone(),
            &audit,
            relay,
            cancel,
        )?;
        publish_result(
            output.as_ref(),
            controls.as_ref(),
            &received.guard,
            &result,
            envelope.operation.result_maximum()?,
        )
    })();
    match attempted {
        Ok(()) => Ok(()),
        Err(error) => publish_failure(output.as_ref(), received.guard, &audit, error),
    }
}

pub fn worker_main() -> Result<()> {
    serve(std::io::stdin(), std::io::stdout())
}

#[cfg(test)]
mod tests;
