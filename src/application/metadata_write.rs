//! Bounded, review-first desktop metadata mutation coordinator.
use super::organization::ImageIdentity;
use super::{BridgeError, Cancellation, ErrorCode, I64, Limits, U64, error, native};
use crate::{
    Catalog, catalog_edits::VariantKey, catalog_metadata::PreparedEdit, storage_volume::NativePath,
    xmp::Edit,
};
use anyhow::{Context, Result as AnyResult, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

type Result<T> = std::result::Result<T, BridgeError>;
const CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteLimits {
    pub input_bytes: U64,
    pub existing_file_bytes: U64,
    pub evidence_bytes: U64,
    pub evidence_packets: U64,
    pub alias_directories: U64,
    pub alias_candidates: U64,
}
impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            input_bytes: U64(1024 * 1024),
            existing_file_bytes: U64(16 * 1024 * 1024),
            evidence_bytes: U64(256 * 1024 * 1024),
            evidence_packets: U64(1024),
            alias_directories: U64(4096),
            alias_candidates: U64(256),
        }
    }
}
impl WriteLimits {
    fn validate(&self) -> AnyResult<()> {
        ensure!(
            (1..=16 * 1024 * 1024).contains(&self.input_bytes.0),
            "metadata input byte limit"
        );
        ensure!(
            (1..=256 * 1024 * 1024).contains(&self.existing_file_bytes.0),
            "existing file byte limit"
        );
        ensure!(
            (1..=256 * 1024 * 1024).contains(&self.evidence_bytes.0),
            "evidence byte limit"
        );
        ensure!(
            (1..=1024).contains(&self.evidence_packets.0),
            "evidence packet limit"
        );
        ensure!(
            self.alias_directories.0 <= 65_536 && (1..=4096).contains(&self.alias_candidates.0),
            "alias limits"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    InputBegin {
        bytes: U64,
        expected_blake3: Option<String>,
        limits: WriteLimits,
    },
    InputAppend {
        token: String,
        generation: U64,
        offset: U64,
        bytes: Vec<u8>,
    },
    InputFinish {
        token: String,
        generation: U64,
        bytes: U64,
        blake3: String,
    },
    InputDiscard {
        token: String,
        generation: U64,
    },
    Prepare {
        identity: ImageIdentity,
        base_model: Option<I64>,
        input: String,
        input_blake3: String,
    },
    Release {
        token: String,
    },
    Commit {
        review: String,
        review_digest: String,
    },
    Resolve {
        identity: ImageIdentity,
        field: String,
        model: I64,
    },
    SidecarPlan {
        identity: ImageIdentity,
        base_model: I64,
        destination: NativePath,
        limits: WriteLimits,
    },
    SidecarApply {
        operation: String,
        authority_blake3: String,
        overwrite_ack: bool,
        limits: WriteLimits,
    },
    SidecarRecover {
        operation: String,
        authority_blake3: String,
        recovery_directory: NativePath,
        may_publish_ack: bool,
        limits: WriteLimits,
    },
    SidecarRestore {
        operation: String,
        authority_blake3: String,
        recovery_directory: NativePath,
        limits: WriteLimits,
    },
    Discover {
        directory: NativePath,
    },
    ReconcilePaths {
        rows: u16,
    },
    EvidenceExport {
        identity: ImageIdentity,
        observation: I64,
        destination: NativePath,
        limits: WriteLimits,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Options,
    Status {
        operation: Option<String>,
    },
    Cancel {
        operation: String,
        epoch: U64,
    },
    Start {
        attempt: String,
        action: Action,
    },
    InputStatus {
        token: String,
        generation: U64,
    },
    Receipt {
        attempt: String,
    },
    Review {
        token: String,
        digest: String,
    },
    ReviewFields {
        token: String,
        digest: String,
        after: Option<String>,
        limit: u16,
    },
    Chunk {
        reference: Reference,
        offset: U64,
        length: u32,
    },
    Plans {
        owner: Option<VariantKey>,
        after: Option<String>,
        limit: u16,
    },
    Plan {
        operation: String,
    },
    RecoveryEntries {
        token: String,
        after: Option<String>,
        limit: u16,
    },
}
impl Request {
    pub fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Options
                | Self::Status { .. }
                | Self::InputStatus { .. }
                | Self::Receipt { .. }
                | Self::Review { .. }
                | Self::ReviewFields { .. }
                | Self::Chunk { .. }
                | Self::Plans { .. }
                | Self::Plan { .. }
                | Self::RecoveryEntries { .. }
        )
    }
    pub fn control(&self) -> bool {
        matches!(self, Self::Status { .. } | Self::Cancel { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Response {
    Options(Options),
    Status(Status),
    Admitted(Admitted),
    Input(Input),
    Receipt(Option<crate::catalog_metadata_write::Receipt>),
    Review(Review),
    ReviewFields(Page<ReviewField>),
    Chunk(Bytes),
    Plans(Page<serde_json::Value>),
    Plan(Option<serde_json::Value>),
    RecoveryEntries(Page<crate::catalog_session::metadata_files::DiscoveryEntry>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub defaults: WriteLimits,
    pub input_max: U64,
    pub packet_max: U64,
    pub edits_max: U64,
    pub page_max: u16,
    pub scan_max: U64,
    pub response_bytes: U64,
    pub chunk_max: u32,
    pub path_bytes: U64,
    pub receipt_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admitted {
    pub operation: String,
    pub attempt: String,
    pub request_digest: String,
    pub epoch: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub token: String,
    pub generation: U64,
    pub bytes: U64,
    pub expected_bytes: U64,
    pub expected_blake3: Option<String>,
    pub last_offset: Option<U64>,
    pub last_chunk_blake3: Option<String>,
    pub sealed: bool,
    pub blake3: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub token: String,
    pub bytes: U64,
    pub blake3: String,
    pub media_type: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub token: String,
    pub digest: String,
    pub identity: ImageIdentity,
    pub base_model: Option<I64>,
    pub input_blake3: String,
    pub edits: U64,
    pub changed_fields: U64,
    pub packet: Reference,
    pub issues: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewField {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub removed: bool,
    pub semantic_changed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub rows: Vec<T>,
    pub next: Option<String>,
    pub scanned: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bytes {
    pub bytes: Vec<u8>,
    pub offset: U64,
    pub total: U64,
    pub next: Option<U64>,
    pub blake3: String,
    pub verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub attempt: String,
    pub request_digest: String,
    pub epoch: U64,
    pub kind: String,
    pub phase: String,
    pub stage: String,
    pub cancel_requested: bool,
    pub progress: U64,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub catalog: String,
    pub epoch: U64,
    pub operation: Option<Operation>,
    pub write_hold: bool,
    pub closing: bool,
    pub input: Option<Input>,
    pub review: Option<Review>,
}

struct InputState {
    dto: Input,
    bytes: Vec<u8>,
}
#[derive(Clone)]
struct Reader {
    reference: Reference,
    bytes: Vec<u8>,
}
struct ReviewState {
    dto: Review,
    prepared: PreparedEdit,
    fields: Vec<ReviewField>,
}
struct DiscoveryState {
    token: String,
    directory: NativePath,
    transfer: crate::catalog_session::LeaseId,
    next_operation: u64,
}
struct WorkerState {
    epoch: u64,
    input: Option<InputState>,
    review: Option<ReviewState>,
    reader: Option<Reader>,
    operation: Option<Operation>,
    discovery: Option<DiscoveryState>,
}
impl Default for WorkerState {
    fn default() -> Self {
        Self {
            epoch: 1,
            input: None,
            review: None,
            reader: None,
            operation: None,
            discovery: None,
        }
    }
}
impl WorkerState {
    pub fn close(&mut self, catalog: &Catalog) -> AnyResult<()> {
        if let Some(discovery) = self.discovery.take() {
            catalog
                .session
                .release_metadata_files(&discovery.transfer, discovery.next_operation)?;
        }
        self.epoch = self.epoch.saturating_add(1);
        self.input = None;
        self.review = None;
        self.reader = None;
        Ok(())
    }
    pub fn execute(
        &mut self,
        catalog_name: &str,
        catalog: &mut Catalog,
        request: Request,
        bounds: &Limits,
        cancel: &Cancellation,
    ) -> Result<Response> {
        match request {
            Request::Options => Ok(Response::Options(options(bounds))),
            Request::Status { operation } => {
                if let Some(id) = operation {
                    valid_uuid(&id)?;
                }
                Ok(Response::Status(self.status(catalog_name)))
            }
            Request::Cancel { operation, epoch } => {
                valid_uuid(&operation)?;
                if epoch.0 != self.epoch {
                    return Err(error(
                        ErrorCode::StaleSession,
                        "metadata operation epoch changed",
                    ));
                }
                if let Some(active) = self
                    .operation
                    .as_mut()
                    .filter(|value| value.id == operation)
                {
                    active.cancel_requested = true;
                }
                Ok(Response::Status(self.status(catalog_name)))
            }
            Request::Start { attempt, action } => {
                self.start(catalog, attempt, action, bounds, cancel)
            }
            Request::InputStatus { token, generation } => {
                let input = self
                    .input
                    .as_ref()
                    .filter(|value| value.dto.token == token && value.dto.generation == generation)
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "metadata input token changed")
                    })?;
                Ok(Response::Input(input.dto.clone()))
            }
            Request::Receipt { attempt } => Ok(Response::Receipt(
                catalog.metadata_write_receipt(&attempt).map_err(native)?,
            )),
            Request::Review { token, digest } => Ok(Response::Review(
                self.checked_review(&token, &digest)?.dto.clone(),
            )),
            Request::ReviewFields {
                token,
                digest,
                after,
                limit,
            } => self.review_fields(&token, &digest, after, limit, bounds),
            Request::Chunk {
                reference,
                offset,
                length,
            } => self.chunk(reference, offset, length),
            Request::Plans {
                owner,
                after,
                limit,
            } => self.plans(catalog, owner.as_ref(), after, limit, bounds),
            Request::Plan { operation } => Ok(Response::Plan(
                plan_json(catalog, &operation).map_err(native)?,
            )),
            Request::RecoveryEntries {
                token,
                after,
                limit,
            } => self.recovery_entries(catalog, token, after, limit, bounds, cancel),
        }
    }

    fn start(
        &mut self,
        catalog: &mut Catalog,
        attempt: String,
        action: Action,
        bounds: &Limits,
        cancel: &Cancellation,
    ) -> Result<Response> {
        crate::catalog_metadata_write::validate_attempt(&attempt).map_err(native)?;
        let action_bytes = serde_json::to_vec(&action).map_err(|value| native(value.into()))?;
        let digest =
            blake3::hash(&[b"photocatalog-metadata-write-v1\0", action_bytes.as_slice()].concat())
                .to_hex()
                .to_string();
        if let Some(receipt) = catalog.metadata_write_receipt(&attempt).map_err(native)? {
            if receipt.request_digest != digest {
                return Err(error(
                    ErrorCode::InvalidRequest,
                    "attempt already belongs to a different request",
                ));
            }
            self.operation = Some(terminal(
                &attempt,
                &digest,
                &kind(&action),
                self.epoch,
                serde_json::to_value(&receipt.result).unwrap(),
            ));
            return Ok(Response::Admitted(Admitted {
                operation: self.operation.as_ref().unwrap().id.clone(),
                attempt,
                request_digest: digest,
                epoch: U64(self.epoch),
            }));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.operation = Some(Operation {
            id: id.clone(),
            attempt: attempt.clone(),
            request_digest: digest.clone(),
            epoch: U64(self.epoch),
            kind: kind(&action),
            phase: "running".into(),
            stage: "admitting".into(),
            cancel_requested: false,
            progress: U64(0),
            result: None,
            error: None,
        });
        let result = self.run(catalog, &attempt, &digest, action, bounds, cancel);
        match result {
            Ok(value) => {
                if let Some(operation) = self.operation.as_mut() {
                    operation.phase = "complete".into();
                    operation.stage = "draining".into();
                    operation.progress = U64(1);
                    operation.result = Some(value);
                }
            }
            Err(failure) => {
                if let Some(operation) = self.operation.as_mut() {
                    operation.phase = if cancel.is_canceled() {
                        "canceled"
                    } else {
                        "failed"
                    }
                    .into();
                    operation.stage = "draining".into();
                    operation.error = Some(failure.message.clone());
                }
                return Err(failure);
            }
        }
        Ok(Response::Admitted(Admitted {
            operation: id,
            attempt,
            request_digest: digest,
            epoch: U64(self.epoch),
        }))
    }

    fn run(
        &mut self,
        catalog: &mut Catalog,
        attempt: &str,
        digest: &str,
        action: Action,
        bounds: &Limits,
        cancel: &Cancellation,
    ) -> Result<serde_json::Value> {
        if cancel.is_canceled() {
            return Err(error(ErrorCode::Canceled, "metadata operation canceled"));
        }
        match action {
            Action::InputBegin {
                bytes,
                expected_blake3,
                limits,
            } => {
                limits.validate().map_err(native)?;
                if bytes.0 > limits.input_bytes.0 {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata input exceeds admitted bytes",
                    ));
                }
                if let Some(value) = &expected_blake3 {
                    digest64(value)?;
                }
                let token = uuid::Uuid::new_v4().to_string();
                let mut body = Vec::new();
                body.try_reserve_exact(
                    usize::try_from(bytes.0).map_err(|value| native(value.into()))?,
                )
                .map_err(|value| native(value.into()))?;
                let dto = Input {
                    token,
                    generation: U64(1),
                    bytes: U64(0),
                    expected_bytes: bytes,
                    expected_blake3,
                    last_offset: None,
                    last_chunk_blake3: None,
                    sealed: false,
                    blake3: None,
                };
                self.input = Some(InputState {
                    dto: dto.clone(),
                    bytes: body,
                });
                self.review = None;
                Ok(tagged("input", &dto))
            }
            Action::InputAppend {
                token,
                generation,
                offset,
                bytes,
            } => {
                if bytes.is_empty() || bytes.len() > CHUNK {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata input chunk must be 1..=16 KiB",
                    ));
                }
                let input = self
                    .input
                    .as_mut()
                    .filter(|value| value.dto.token == token && value.dto.generation == generation)
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "metadata input token changed")
                    })?;
                if input.dto.sealed
                    || offset.0 != input.bytes.len() as u64
                    || input.bytes.len().saturating_add(bytes.len())
                        > input.dto.expected_bytes.0 as usize
                {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "metadata input offset or extent differs",
                    ));
                }
                input.dto.last_offset = Some(offset);
                input.dto.last_chunk_blake3 = Some(blake3::hash(&bytes).to_hex().to_string());
                input.bytes.extend_from_slice(&bytes);
                input.dto.bytes = U64(input.bytes.len() as u64);
                Ok(tagged("input", &input.dto))
            }
            Action::InputFinish {
                token,
                generation,
                bytes,
                blake3,
            } => {
                digest64(&blake3)?;
                let input = self
                    .input
                    .as_mut()
                    .filter(|value| value.dto.token == token && value.dto.generation == generation)
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "metadata input token changed")
                    })?;
                let actual = blake3::hash(&input.bytes).to_hex().to_string();
                if bytes.0 != input.bytes.len() as u64
                    || bytes != input.dto.expected_bytes
                    || actual != blake3
                    || input
                        .dto
                        .expected_blake3
                        .as_ref()
                        .is_some_and(|value| value != &actual)
                {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "metadata input finish proof differs",
                    ));
                }
                std::str::from_utf8(&input.bytes).map_err(|_| {
                    error(ErrorCode::InvalidRequest, "metadata input must be UTF-8")
                })?;
                input.dto.sealed = true;
                input.dto.blake3 = Some(actual);
                Ok(tagged("input", &input.dto))
            }
            Action::InputDiscard { token, generation } => {
                let input = self
                    .input
                    .as_ref()
                    .filter(|value| value.dto.token == token && value.dto.generation == generation)
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "metadata input token changed")
                    })?;
                if self.review.as_ref().is_some_and(|value| {
                    value.dto.input_blake3 == input.dto.blake3.clone().unwrap_or_default()
                }) {
                    return Err(error(
                        ErrorCode::Busy,
                        "release the prepared review before discarding its input",
                    ));
                }
                self.input = None;
                Ok(tagged("released", &()))
            }
            Action::Prepare {
                identity,
                base_model,
                input,
                input_blake3,
            } => {
                digest64(&input_blake3)?;
                let staged = self
                    .input
                    .as_ref()
                    .filter(|value| {
                        value.dto.token == input
                            && value.dto.sealed
                            && value.dto.blake3.as_deref() == Some(input_blake3.as_str())
                    })
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "sealed metadata input changed")
                    })?;
                let edits: Vec<Edit> = serde_json::from_slice(&staged.bytes)
                    .map_err(|value| error(ErrorCode::InvalidRequest, value.to_string()))?;
                if edits.is_empty() || edits.len() > 1000 {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata edit count must be 1..=1000",
                    ));
                }
                let core_identity: crate::catalog_images::ImageMetadataIdentity =
                    identity.clone().into();
                let prepared = catalog
                    .prepare_metadata_edit(
                        &core_identity.image_id,
                        core_identity.metadata_revision,
                        base_model.map(|value| value.0),
                        &edits,
                        &[],
                    )
                    .map_err(native)?;
                let summary = prepared.summary().map_err(native)?;
                if summary.identity != core_identity {
                    return Err(error(
                        ErrorCode::StaleSession,
                        "selected metadata identity changed",
                    ));
                }
                let packet = prepared.packet().map_err(native)?;
                let reference = Reference {
                    token: uuid::Uuid::new_v4().to_string(),
                    bytes: U64(packet.len() as u64),
                    blake3: summary.packet_blake3.clone(),
                    media_type: "application/rdf+xml".into(),
                };
                self.reader = Some(Reader {
                    reference: reference.clone(),
                    bytes: packet,
                });
                let mut fields = Vec::new();
                for field in summary
                    .before
                    .fields
                    .keys()
                    .chain(summary.after.fields.keys())
                {
                    if fields
                        .iter()
                        .any(|value: &ReviewField| &value.field == field)
                    {
                        continue;
                    }
                    let before = summary
                        .before
                        .fields
                        .get(field)
                        .map(|value| serde_json::to_string(value).unwrap());
                    let after = summary
                        .after
                        .fields
                        .get(field)
                        .map(|value| serde_json::to_string(value).unwrap());
                    if before != after {
                        fields.push(ReviewField {
                            field: field.clone(),
                            removed: matches!(
                                summary.after.fields.get(field),
                                Some(crate::xmp::Value::Removed)
                            ) || !summary.after.fields.contains_key(field),
                            semantic_changed: true,
                            before,
                            after,
                        });
                    }
                }
                let review_digest = blake3::hash(
                    &serde_json::to_vec(&(&summary, &input_blake3))
                        .map_err(|value| native(value.into()))?,
                )
                .to_hex()
                .to_string();
                let dto = Review {
                    token: uuid::Uuid::new_v4().to_string(),
                    digest: review_digest,
                    identity,
                    base_model,
                    input_blake3,
                    edits: U64(edits.len() as u64),
                    changed_fields: U64(fields.len() as u64),
                    packet: reference,
                    issues: serde_json::to_string(&summary.issues).unwrap(),
                };
                self.review = Some(ReviewState {
                    dto: dto.clone(),
                    prepared,
                    fields,
                });
                Ok(tagged("review", &dto))
            }
            Action::Release { token } => {
                if self
                    .review
                    .as_ref()
                    .is_some_and(|value| value.dto.token == token)
                {
                    self.review = None;
                    self.reader = None;
                } else if self
                    .input
                    .as_ref()
                    .is_some_and(|value| value.dto.token == token)
                {
                    self.input = None;
                } else if self
                    .discovery
                    .as_ref()
                    .is_some_and(|value| value.token == token)
                {
                    let discovery = self.discovery.take().unwrap();
                    catalog
                        .session
                        .release_metadata_files(&discovery.transfer, discovery.next_operation)
                        .map_err(native)?;
                } else {
                    return Err(error(ErrorCode::StaleSession, "metadata token changed"));
                }
                Ok(tagged("released", &()))
            }
            Action::Commit {
                review,
                review_digest,
            } => {
                let prepared = self
                    .review
                    .take()
                    .filter(|value| value.dto.token == review && value.dto.digest == review_digest)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "metadata review changed"))?
                    .prepared;
                self.input = None;
                self.reader = None;
                let change = catalog
                    .commit_prepared_metadata_edit_with_receipt(prepared, attempt, digest)
                    .map_err(native)?;
                Ok(tagged("changed", &change))
            }
            Action::Resolve {
                identity,
                field,
                model,
            } => {
                let identity: crate::catalog_images::ImageMetadataIdentity = identity.into();
                let revision = catalog
                    .resolve_metadata_for_image_with_receipt(
                        &identity, &field, model.0, attempt, digest,
                    )
                    .map_err(native)?;
                Ok(tagged(
                    "resolved",
                    &serde_json::json!({"revision": I64(revision)}),
                ))
            }
            Action::SidecarPlan {
                identity,
                base_model,
                destination,
                limits,
            } => {
                limits.validate().map_err(native)?;
                let path = destination
                    .to_path()
                    .map_err(|value| native(value.into()))?;
                let core_identity: crate::catalog_images::ImageMetadataIdentity =
                    identity.clone().into();
                let plan = catalog
                    .plan_image_metadata_export_with_receipt(
                        &core_identity.key,
                        core_identity.metadata_revision,
                        base_model.0,
                        &path,
                        &cancel.0,
                        attempt,
                        digest,
                    )
                    .map_err(native)?;
                Ok(tagged(
                    "sidecar_plan",
                    &plan_json(catalog, &plan.export.destination.operation)
                        .map_err(native)?
                        .context("stored sidecar plan missing")
                        .map_err(native)?,
                ))
            }
            Action::SidecarApply {
                operation,
                authority_blake3,
                overwrite_ack,
                limits,
            } => {
                limits.validate().map_err(native)?;
                let (_, _, _, plan, _, _) = checked_plan(catalog, &operation, &authority_blake3)?;
                if plan.expected.is_some() && !overwrite_ack {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "review and acknowledge the exact existing destination",
                    ));
                }
                let receipt = catalog
                    .apply_metadata_export_with_receipt(
                        &operation,
                        &cancel.0,
                        attempt,
                        digest,
                        "sidecar_apply",
                    )
                    .map_err(native)?;
                Ok(tagged("sidecar_receipt", &receipt))
            }
            Action::SidecarRecover {
                operation,
                authority_blake3,
                recovery_directory,
                may_publish_ack,
                limits,
            } => {
                limits.validate().map_err(native)?;
                checked_plan(catalog, &operation, &authority_blake3)?;
                if !may_publish_ack {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "recovery can publish when authority is current; acknowledge before continuing",
                    ));
                }
                let path = recovery_directory
                    .to_path()
                    .map_err(|value| native(value.into()))?;
                let receipt = catalog
                    .recover_metadata_export_with_receipt(&path, false, &cancel.0, attempt, digest)
                    .map_err(native)?;
                Ok(tagged("sidecar_receipt", &receipt))
            }
            Action::SidecarRestore {
                operation,
                authority_blake3,
                recovery_directory,
                limits,
            } => {
                limits.validate().map_err(native)?;
                checked_plan(catalog, &operation, &authority_blake3)?;
                let path = recovery_directory
                    .to_path()
                    .map_err(|value| native(value.into()))?;
                let receipt = catalog
                    .recover_metadata_export_with_receipt(&path, true, &cancel.0, attempt, digest)
                    .map_err(native)?;
                Ok(tagged("sidecar_receipt", &receipt))
            }
            Action::Discover { directory } => {
                if let Some(discovery) = self.discovery.take() {
                    catalog
                        .session
                        .release_metadata_files(&discovery.transfer, discovery.next_operation)
                        .map_err(native)?;
                }
                let token = uuid::Uuid::new_v4().to_string();
                self.discovery = Some(DiscoveryState {
                    token: token.clone(),
                    directory: directory.clone(),
                    transfer: crate::catalog_session::LeaseId::new(),
                    next_operation: 1,
                });
                Ok(tagged(
                    "discovery",
                    &serde_json::json!({"token":token,"directory":directory}),
                ))
            }
            Action::ReconcilePaths { rows } => {
                if rows == 0 || usize::from(rows) > bounds.scan_rows {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata path reconciliation row limit",
                    ));
                }
                let progress = catalog
                    .reconcile_export_paths(usize::from(rows))
                    .map_err(native)?;
                Ok(tagged(
                    "paths",
                    &serde_json::json!({"projected":U64(progress.projected as u64),"pending":U64(u64::from(progress.pending)),"unbound":U64(progress.unbound)}),
                ))
            }
            Action::EvidenceExport {
                identity,
                observation,
                destination,
                limits,
            } => {
                limits.validate().map_err(native)?;
                let identity: crate::catalog_images::ImageMetadataIdentity = identity.into();
                let receipt = catalog
                    .export_metadata_evidence(
                        &identity,
                        observation.0,
                        &destination,
                        limits.evidence_bytes.0,
                        usize::try_from(limits.evidence_packets.0)
                            .map_err(|value| native(value.into()))?,
                        &cancel.0,
                    )
                    .map_err(native)?;
                Ok(tagged(
                    "evidence",
                    &serde_json::json!({"destination":receipt.destination,"bytes":receipt.bytes,"blake3":receipt.blake3,"state":"complete","detail":"Exact retained packet document created."}),
                ))
            }
        }
    }

    fn checked_review(&self, token: &str, digest: &str) -> Result<&ReviewState> {
        self.review
            .as_ref()
            .filter(|value| value.dto.token == token && value.dto.digest == digest)
            .ok_or_else(|| error(ErrorCode::StaleSession, "metadata review changed"))
    }
    fn status(&self, catalog: &str) -> Status {
        Status {
            catalog: catalog.into(),
            epoch: U64(self.epoch),
            operation: self.operation.clone(),
            write_hold: self.operation.as_ref().is_some_and(|value| {
                value.phase == "running"
                    && matches!(
                        value.stage.as_str(),
                        "waiting_writer" | "committing" | "capturing" | "publishing" | "restoring"
                    )
            }),
            closing: false,
            input: self.input.as_ref().map(|value| value.dto.clone()),
            review: self.review.as_ref().map(|value| value.dto.clone()),
        }
    }
    fn review_fields(
        &self,
        token: &str,
        digest: &str,
        after: Option<String>,
        limit: u16,
        bounds: &Limits,
    ) -> Result<Response> {
        if limit == 0 || limit > bounds.page_rows {
            return Err(error(
                ErrorCode::ResourceLimit,
                "metadata review page limit",
            ));
        }
        let review = self.checked_review(token, digest)?;
        let start = after
            .as_deref()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|_| error(ErrorCode::InvalidRequest, "invalid review cursor"))?
            .unwrap_or(0);
        let end = (start + usize::from(limit)).min(review.fields.len());
        Ok(Response::ReviewFields(Page {
            rows: review.fields[start..end].to_vec(),
            next: (end < review.fields.len()).then(|| end.to_string()),
            scanned: U64((end - start) as u64),
        }))
    }
    fn chunk(&self, reference: Reference, offset: U64, length: u32) -> Result<Response> {
        if length == 0 || length as usize > CHUNK {
            return Err(error(ErrorCode::ResourceLimit, "metadata chunk limit"));
        }
        let reader = self
            .reader
            .as_ref()
            .filter(|value| {
                value.reference.token == reference.token
                    && value.reference.blake3 == reference.blake3
                    && value.reference.bytes == reference.bytes
            })
            .ok_or_else(|| error(ErrorCode::StaleSession, "metadata byte reference changed"))?;
        let start = usize::try_from(offset.0).map_err(|value| native(value.into()))?;
        if start > reader.bytes.len() {
            return Err(error(ErrorCode::InvalidRequest, "metadata chunk offset"));
        }
        let end = (start + length as usize).min(reader.bytes.len());
        Ok(Response::Chunk(Bytes {
            bytes: reader.bytes[start..end].to_vec(),
            offset,
            total: U64(reader.bytes.len() as u64),
            next: (end < reader.bytes.len()).then(|| U64(end as u64)),
            blake3: reader.reference.blake3.clone(),
            verified: true,
        }))
    }
    fn plans(
        &self,
        catalog: &Catalog,
        owner: Option<&VariantKey>,
        after: Option<String>,
        limit: u16,
        bounds: &Limits,
    ) -> Result<Response> {
        if limit == 0 || limit > bounds.page_rows {
            return Err(error(ErrorCode::ResourceLimit, "metadata plan page limit"));
        }
        let cursor = after
            .as_deref()
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| error(ErrorCode::InvalidRequest, "invalid metadata plan cursor"))?
            .unwrap_or(0);
        let (rows, next, scanned) = catalog
            .metadata_export_plans(
                owner,
                cursor,
                usize::from(limit),
                bounds.scan_rows.min(1000),
            )
            .map_err(native)?;
        let rows = rows
            .into_iter()
            .map(
                |(rowid, authority, revision, base_model, plan, receipt, authority_digest)| {
                    sidecar_json(
                        authority,
                        revision,
                        base_model,
                        plan,
                        receipt,
                        authority_digest,
                        rowid,
                    )
                },
            )
            .collect();
        Ok(Response::Plans(Page {
            rows,
            next: next.map(|value| value.to_string()),
            scanned: U64(scanned as u64),
        }))
    }
    fn recovery_entries(
        &mut self,
        catalog: &Catalog,
        token: String,
        after: Option<String>,
        limit: u16,
        bounds: &Limits,
        cancel: &Cancellation,
    ) -> Result<Response> {
        let discovery = self
            .discovery
            .as_mut()
            .filter(|value| value.token == token)
            .ok_or_else(|| error(ErrorCode::StaleSession, "metadata discovery token changed"))?;
        if limit == 0 || limit > bounds.page_rows {
            return Err(error(
                ErrorCode::ResourceLimit,
                "metadata recovery page limit",
            ));
        }
        let after = after
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| error(ErrorCode::InvalidRequest, "invalid recovery cursor"))
            })
            .transpose()?;
        let value = catalog
            .session
            .discover_metadata_files(
                &discovery.transfer,
                discovery.next_operation,
                &discovery.directory,
                after,
                bounds.scan_rows.min(1000) as u64,
                limit as u64,
                &cancel.0,
            )
            .map_err(native)?;
        discovery.next_operation = discovery.next_operation.checked_add(1).ok_or_else(|| {
            error(
                ErrorCode::ResourceLimit,
                "metadata discovery operation exhausted",
            )
        })?;
        match value {
            Some(crate::catalog_session::metadata_files::Value::Discovery {
                rows,
                next,
                scanned,
            }) => Ok(Response::RecoveryEntries(Page {
                rows,
                next: next.map(|path| serde_json::to_string(&path).unwrap()),
                scanned,
            })),
            None => Err(error(
                ErrorCode::Native,
                "recovery discovery requires managed filesystem custody",
            )),
            _ => Err(error(
                ErrorCode::Native,
                "unexpected recovery discovery result",
            )),
        }
    }
}

#[derive(Default)]
struct SharedState {
    epoch: u64,
    operation: Option<Operation>,
    input: Option<Input>,
    review: Option<Review>,
    reader: Option<Reader>,
    fields: Vec<ReviewField>,
    cancel: Option<Cancellation>,
    write_hold: bool,
    closing: bool,
}

enum WorkerTask {
    Run {
        operation: Operation,
        action: Action,
        bounds: Limits,
        cancel: Cancellation,
        write_ready: mpsc::Receiver<()>,
    },
    RecoveryEntries {
        token: String,
        after: Option<String>,
        limit: u16,
        bounds: Limits,
        cancel: Cancellation,
        reply: mpsc::SyncSender<Result<Response>>,
    },
    Shutdown,
}

struct Worker {
    sender: mpsc::Sender<WorkerTask>,
    join: thread::JoinHandle<()>,
    session: Arc<crate::catalog_session::CatalogSessionAuthority>,
}

pub struct Coordinator {
    shared: Arc<Mutex<SharedState>>,
    worker: Option<Worker>,
    write_ready: Option<mpsc::SyncSender<()>>,
    pause: Option<crate::preview::NativeLaunchPause>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(SharedState {
                epoch: 1,
                ..SharedState::default()
            })),
            worker: None,
            write_ready: None,
            pause: None,
        }
    }
}

impl Coordinator {
    pub fn write_hold(&self) -> bool {
        self.shared.lock().unwrap().write_hold
    }

    pub fn needs_write(&self) -> bool {
        self.shared.lock().unwrap().write_hold && self.write_ready.is_some()
    }

    pub fn request_cancel(&self) {
        if let Some(cancel) = &self.shared.lock().unwrap().cancel {
            cancel.cancel();
        }
    }

    pub fn start_write(&mut self, service: &mut crate::preview::PreviewService) -> Result<()> {
        if self.pause.is_none() {
            self.pause = Some(service.pause_native_launches().map_err(native)?);
        }
        if let Some(ready) = self.write_ready.take() {
            ready
                .send(())
                .map_err(|_| error(ErrorCode::Native, "metadata write handshake was lost"))?;
        }
        Ok(())
    }

    pub fn release_completed(&mut self) -> Result<()> {
        if !self.shared.lock().unwrap().write_hold {
            self.pause = None;
            self.write_ready = None;
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.join.is_finished())
        {
            let worker = self.worker.take().unwrap();
            let healthy = worker.join.join().is_ok();
            worker
                .session
                .joined(crate::catalog_session::SqlRole::Relink, healthy)
                .map_err(native)?;
            if !healthy {
                return Err(error(ErrorCode::Native, "metadata worker panicked"));
            }
        }
        Ok(())
    }

    fn ensure_worker(&mut self, catalog: &Catalog) -> Result<()> {
        if self.worker.is_some() {
            return Ok(());
        }
        let handle = catalog.relink_worker_handle().map_err(native)?;
        let session = catalog.session.clone();
        let shared = Arc::clone(&self.shared);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("catalog-metadata-write".into())
            .spawn(move || {
                let mut catalog = match handle.open() {
                    Ok(catalog) => catalog,
                    Err(failure) => {
                        let mut shared = shared.lock().unwrap();
                        if let Some(operation) = shared.operation.as_mut() {
                            operation.phase = "failed".into();
                            operation.stage = "draining".into();
                            operation.error = Some(format!(
                                "metadata worker could not open the selected catalog: {failure:#}"
                            ));
                        }
                        shared.write_hold = false;
                        shared.cancel = None;
                        return;
                    }
                };
                let mut state = WorkerState::default();
                while let Ok(task) = receiver.recv() {
                    match task {
                        WorkerTask::Run {
                            mut operation,
                            action,
                            bounds,
                            cancel,
                            write_ready,
                        } => {
                            let result = if action_write_hold(&action) {
                                loop {
                                    if cancel.is_canceled() {
                                        break Err(error(
                                            ErrorCode::Canceled,
                                            "metadata operation canceled before writer admission",
                                        ));
                                    }
                                    match write_ready
                                        .recv_timeout(std::time::Duration::from_millis(50))
                                    {
                                        Ok(()) => {
                                            break state.run(
                                                &mut catalog,
                                                &operation.attempt,
                                                &operation.request_digest,
                                                action,
                                                &bounds,
                                                &cancel,
                                            );
                                        }
                                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                                            break Err(error(
                                                ErrorCode::Closed,
                                                "metadata writer admission was withdrawn",
                                            ));
                                        }
                                    }
                                }
                            } else {
                                state.run(
                                    &mut catalog,
                                    &operation.attempt,
                                    &operation.request_digest,
                                    action,
                                    &bounds,
                                    &cancel,
                                )
                            };
                            match result {
                                Ok(value) => {
                                    operation.phase = "complete".into();
                                    operation.progress = U64(1);
                                    operation.result = Some(value);
                                }
                                Err(failure) => {
                                    operation.phase = if cancel.is_canceled() {
                                        "canceled"
                                    } else {
                                        "failed"
                                    }
                                    .into();
                                    operation.error = Some(failure.message);
                                }
                            }
                            operation.stage = "draining".into();
                            let mut shared = shared.lock().unwrap();
                            shared.operation = Some(operation);
                            shared.input = state.input.as_ref().map(|value| value.dto.clone());
                            shared.review = state.review.as_ref().map(|value| value.dto.clone());
                            shared.reader = state.reader.clone();
                            shared.fields = state
                                .review
                                .as_ref()
                                .map(|value| value.fields.clone())
                                .unwrap_or_default();
                            shared.write_hold = false;
                            shared.cancel = None;
                            let idle = state.input.is_none()
                                && state.review.is_none()
                                && state.reader.is_none()
                                && state.discovery.is_none();
                            drop(shared);
                            if idle {
                                break;
                            }
                        }
                        WorkerTask::RecoveryEntries {
                            token,
                            after,
                            limit,
                            bounds,
                            cancel,
                            reply,
                        } => {
                            let result = state
                                .recovery_entries(&catalog, token, after, limit, &bounds, &cancel);
                            let _ = reply.send(result);
                        }
                        WorkerTask::Shutdown => break,
                    }
                }
                let _ = state.close(&catalog);
            })
            .map_err(|value| native(value.into()))?;
        self.worker = Some(Worker {
            sender,
            join,
            session,
        });
        Ok(())
    }

    pub fn close(&mut self, _catalog: &Catalog) -> AnyResult<()> {
        {
            let mut shared = self.shared.lock().unwrap();
            shared.closing = true;
            if let Some(cancel) = &shared.cancel {
                cancel.cancel();
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.sender.send(WorkerTask::Shutdown);
            let healthy = worker.join.join().is_ok();
            worker
                .session
                .joined(crate::catalog_session::SqlRole::Relink, healthy)?;
            ensure!(healthy, "metadata worker panicked during Close");
        }
        let mut shared = self.shared.lock().unwrap();
        shared.epoch = shared.epoch.saturating_add(1);
        shared.input = None;
        shared.review = None;
        shared.reader = None;
        shared.fields.clear();
        shared.cancel = None;
        shared.write_hold = false;
        self.write_ready = None;
        self.pause = None;
        Ok(())
    }

    pub fn execute(
        &mut self,
        catalog_name: &str,
        catalog: &mut Catalog,
        request: Request,
        bounds: &Limits,
        _command_cancel: &Cancellation,
    ) -> Result<Response> {
        match request {
            Request::Options => Ok(Response::Options(options(bounds))),
            Request::Status { operation } => {
                if let Some(operation) = &operation {
                    valid_uuid(operation)?;
                }
                let shared = self.shared.lock().unwrap();
                if operation.as_ref().is_some_and(|expected| {
                    shared.operation.as_ref().map(|value| &value.id) != Some(expected)
                }) {
                    return Err(error(ErrorCode::StaleSession, "metadata operation changed"));
                }
                Ok(Response::Status(Status {
                    catalog: catalog_name.into(),
                    epoch: U64(shared.epoch),
                    operation: shared.operation.clone(),
                    write_hold: shared.write_hold,
                    closing: shared.closing,
                    input: shared.input.clone(),
                    review: shared.review.clone(),
                }))
            }
            Request::Cancel { operation, epoch } => {
                valid_uuid(&operation)?;
                let mut shared = self.shared.lock().unwrap();
                if epoch.0 != shared.epoch
                    || shared.operation.as_ref().map(|value| &value.id) != Some(&operation)
                {
                    return Err(error(ErrorCode::StaleSession, "metadata operation changed"));
                }
                if let Some(cancel) = &shared.cancel {
                    cancel.cancel();
                    if let Some(operation) = shared.operation.as_mut() {
                        operation.cancel_requested = true;
                    }
                }
                Ok(Response::Status(Status {
                    catalog: catalog_name.into(),
                    epoch: U64(shared.epoch),
                    operation: shared.operation.clone(),
                    write_hold: shared.write_hold,
                    closing: shared.closing,
                    input: shared.input.clone(),
                    review: shared.review.clone(),
                }))
            }
            Request::Start { attempt, action } => {
                self.release_completed()?;
                crate::catalog_metadata_write::validate_attempt(&attempt).map_err(native)?;
                let digest = canonical_request_digest(catalog, &self.shared, &action)?;
                if let Some(receipt) = catalog.metadata_write_receipt(&attempt).map_err(native)? {
                    if receipt.request_digest != digest {
                        return Err(error(
                            ErrorCode::InvalidRequest,
                            "attempt already belongs to a different request",
                        ));
                    }
                    let epoch = self.shared.lock().unwrap().epoch;
                    let operation =
                        terminal(&attempt, &digest, &kind(&action), epoch, receipt.result);
                    let id = operation.id.clone();
                    self.shared.lock().unwrap().operation = Some(operation);
                    return Ok(Response::Admitted(Admitted {
                        operation: id,
                        attempt,
                        request_digest: digest,
                        epoch: U64(epoch),
                    }));
                }
                {
                    let shared = self.shared.lock().unwrap();
                    if shared.closing {
                        return Err(error(ErrorCode::Closed, "metadata worker is closing"));
                    }
                    if shared.operation.as_ref().is_some_and(|value| {
                        !matches!(value.phase.as_str(), "complete" | "failed" | "canceled")
                    }) {
                        return Err(error(
                            ErrorCode::Busy,
                            "metadata operation is still running or draining",
                        ));
                    }
                }
                self.ensure_worker(catalog)?;
                let epoch = self.shared.lock().unwrap().epoch;
                let id = uuid::Uuid::new_v4().to_string();
                let write_hold = action_write_hold(&action);
                let operation = Operation {
                    id: id.clone(),
                    attempt: attempt.clone(),
                    request_digest: digest.clone(),
                    epoch: U64(epoch),
                    kind: kind(&action),
                    phase: "running".into(),
                    stage: if write_hold {
                        "waiting_writer"
                    } else {
                        "preparing"
                    }
                    .into(),
                    cancel_requested: false,
                    progress: U64(0),
                    result: None,
                    error: None,
                };
                let cancel = Cancellation::default();
                let (write_ready, wait_for_write) = mpsc::sync_channel(1);
                {
                    let mut shared = self.shared.lock().unwrap();
                    shared.operation = Some(operation.clone());
                    shared.cancel = Some(cancel.clone());
                    shared.write_hold = write_hold;
                }
                self.write_ready = write_hold.then_some(write_ready);
                self.worker
                    .as_ref()
                    .unwrap()
                    .sender
                    .send(WorkerTask::Run {
                        operation,
                        action,
                        bounds: bounds.clone(),
                        cancel,
                        write_ready: wait_for_write,
                    })
                    .map_err(|_| {
                        error(
                            ErrorCode::Native,
                            "metadata worker stopped before admission was delivered",
                        )
                    })?;
                Ok(Response::Admitted(Admitted {
                    operation: id,
                    attempt,
                    request_digest: digest,
                    epoch: U64(epoch),
                }))
            }
            Request::InputStatus { token, generation } => {
                let shared = self.shared.lock().unwrap();
                let input = shared
                    .input
                    .as_ref()
                    .filter(|value| value.token == token && value.generation == generation)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "metadata input changed"))?;
                Ok(Response::Input(input.clone()))
            }
            Request::Receipt { attempt } => Ok(Response::Receipt(
                catalog.metadata_write_receipt(&attempt).map_err(native)?,
            )),
            Request::Review { token, digest } => {
                let shared = self.shared.lock().unwrap();
                let review = shared
                    .review
                    .as_ref()
                    .filter(|value| value.token == token && value.digest == digest)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "metadata review changed"))?;
                Ok(Response::Review(review.clone()))
            }
            Request::ReviewFields {
                token,
                digest,
                after,
                limit,
            } => {
                let shared = self.shared.lock().unwrap();
                shared
                    .review
                    .as_ref()
                    .filter(|value| value.token == token && value.digest == digest)
                    .ok_or_else(|| error(ErrorCode::StaleSession, "metadata review changed"))?;
                if limit == 0 || limit > bounds.page_rows {
                    return Err(error(
                        ErrorCode::ResourceLimit,
                        "metadata review page limit",
                    ));
                }
                let start = after
                    .as_deref()
                    .map(str::parse::<usize>)
                    .transpose()
                    .map_err(|_| error(ErrorCode::InvalidRequest, "invalid review cursor"))?
                    .unwrap_or(0);
                let end = start
                    .saturating_add(limit as usize)
                    .min(shared.fields.len());
                Ok(Response::ReviewFields(Page {
                    rows: shared.fields[start..end].to_vec(),
                    next: (end < shared.fields.len()).then(|| end.to_string()),
                    scanned: U64((end - start) as u64),
                }))
            }
            Request::Chunk {
                reference,
                offset,
                length,
            } => {
                if length == 0 || length as usize > CHUNK {
                    return Err(error(ErrorCode::ResourceLimit, "metadata chunk limit"));
                }
                let shared = self.shared.lock().unwrap();
                let reader = shared
                    .reader
                    .as_ref()
                    .filter(|value| {
                        value.reference.token == reference.token
                            && value.reference.blake3 == reference.blake3
                            && value.reference.bytes == reference.bytes
                    })
                    .ok_or_else(|| {
                        error(ErrorCode::StaleSession, "metadata byte reference changed")
                    })?;
                let start = usize::try_from(offset.0).map_err(|value| native(value.into()))?;
                if start > reader.bytes.len() {
                    return Err(error(ErrorCode::InvalidRequest, "metadata chunk offset"));
                }
                let end = start
                    .saturating_add(length as usize)
                    .min(reader.bytes.len());
                Ok(Response::Chunk(Bytes {
                    bytes: reader.bytes[start..end].to_vec(),
                    offset,
                    total: U64(reader.bytes.len() as u64),
                    next: (end < reader.bytes.len()).then(|| U64(end as u64)),
                    blake3: reader.reference.blake3.clone(),
                    verified: true,
                }))
            }
            Request::Plans {
                owner,
                after,
                limit,
            } => WorkerState::default().plans(catalog, owner.as_ref(), after, limit, bounds),
            Request::Plan { operation } => Ok(Response::Plan(
                plan_json(catalog, &operation).map_err(native)?,
            )),
            Request::RecoveryEntries {
                token,
                after,
                limit,
            } => {
                let worker = self.worker.as_ref().ok_or_else(|| {
                    error(ErrorCode::StaleSession, "metadata discovery worker changed")
                })?;
                let cancel = Cancellation::default();
                let (reply, receive) = mpsc::sync_channel(1);
                worker
                    .sender
                    .send(WorkerTask::RecoveryEntries {
                        token,
                        after,
                        limit,
                        bounds: bounds.clone(),
                        cancel,
                        reply,
                    })
                    .map_err(|_| error(ErrorCode::Native, "metadata worker stopped"))?;
                receive
                    .recv()
                    .map_err(|_| error(ErrorCode::Native, "metadata worker response was lost"))?
            }
        }
    }
}

pub(super) fn action_write_hold(action: &Action) -> bool {
    matches!(
        action,
        Action::Commit { .. }
            | Action::Resolve { .. }
            | Action::SidecarPlan { .. }
            | Action::SidecarApply { .. }
            | Action::SidecarRecover { .. }
            | Action::SidecarRestore { .. }
            | Action::EvidenceExport { .. }
    )
}

fn canonical_request_digest(
    catalog: &Catalog,
    shared: &Arc<Mutex<SharedState>>,
    action: &Action,
) -> Result<String> {
    let authority = match action {
        Action::Commit {
            review,
            review_digest,
        } => {
            let shared = shared.lock().unwrap();
            let value = shared
                .review
                .as_ref()
                .filter(|value| value.token == *review && value.digest == *review_digest)
                .ok_or_else(|| error(ErrorCode::StaleSession, "metadata review changed"))?;
            serde_json::to_vec(&(
                "edit",
                &value.identity,
                value.base_model,
                &value.input_blake3,
                review_digest,
            ))
        }
        Action::Resolve {
            identity,
            field,
            model,
        } => serde_json::to_vec(&("resolve", identity, field, model)),
        Action::SidecarPlan {
            identity,
            base_model,
            destination,
            limits,
        } => serde_json::to_vec(&("sidecar_plan", identity, base_model, destination, limits)),
        Action::SidecarApply {
            operation,
            authority_blake3,
            overwrite_ack,
            limits,
        } => {
            let exact = catalog
                .metadata_export_plan(operation)
                .map_err(native)?
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "metadata plan not found"))?
                .5;
            if &exact != authority_blake3 {
                return Err(error(
                    ErrorCode::StaleSession,
                    "metadata plan authority changed",
                ));
            }
            serde_json::to_vec(&("sidecar_apply", operation, exact, overwrite_ack, limits))
        }
        Action::SidecarRecover {
            operation,
            authority_blake3,
            recovery_directory,
            may_publish_ack,
            limits,
        } => {
            let exact = catalog
                .metadata_export_plan(operation)
                .map_err(native)?
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "metadata plan not found"))?
                .5;
            if &exact != authority_blake3 {
                return Err(error(
                    ErrorCode::StaleSession,
                    "metadata plan authority changed",
                ));
            }
            serde_json::to_vec(&(
                "sidecar_recover",
                operation,
                exact,
                recovery_directory,
                may_publish_ack,
                limits,
            ))
        }
        Action::SidecarRestore {
            operation,
            authority_blake3,
            recovery_directory,
            limits,
        } => {
            let exact = catalog
                .metadata_export_plan(operation)
                .map_err(native)?
                .ok_or_else(|| error(ErrorCode::InvalidRequest, "metadata plan not found"))?
                .5;
            if &exact != authority_blake3 {
                return Err(error(
                    ErrorCode::StaleSession,
                    "metadata plan authority changed",
                ));
            }
            serde_json::to_vec(&(
                "sidecar_restore",
                operation,
                exact,
                recovery_directory,
                limits,
            ))
        }
        _ => serde_json::to_vec(action),
    }
    .map_err(|value| native(value.into()))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"photocatalog-metadata-write-v1\0");
    hasher.update(&authority);
    Ok(hasher.finalize().to_hex().to_string())
}

fn options(bounds: &Limits) -> Options {
    Options {
        defaults: WriteLimits::default(),
        input_max: U64(16 * 1024 * 1024),
        packet_max: U64(16 * 1024 * 1024),
        edits_max: U64(1000),
        page_max: bounds.page_rows.min(100),
        scan_max: U64(bounds.scan_rows.min(1000) as u64),
        response_bytes: U64(bounds.reply_bytes.min(256 * 1024) as u64),
        chunk_max: CHUNK as u32,
        path_bytes: U64(32 * 1024),
        receipt_bytes: U64(64 * 1024),
    }
}
fn valid_uuid(value: &str) -> Result<()> {
    uuid::Uuid::parse_str(value)
        .map_err(|_| error(ErrorCode::InvalidRequest, "invalid operation UUID"))?;
    Ok(())
}
fn digest64(value: &str) -> Result<()> {
    crate::catalog_metadata_write::validate_digest(value).map_err(native)
}
fn kind(action: &Action) -> String {
    match action {
        Action::InputBegin { .. } => "input_begin",
        Action::InputAppend { .. } => "input_append",
        Action::InputFinish { .. } => "input_finish",
        Action::InputDiscard { .. } => "input_discard",
        Action::Prepare { .. } => "prepare",
        Action::Release { .. } => "release",
        Action::Commit { .. } => "commit",
        Action::Resolve { .. } => "resolve",
        Action::SidecarPlan { .. } => "sidecar_plan",
        Action::SidecarApply { .. } => "sidecar_apply",
        Action::SidecarRecover { .. } => "sidecar_recover",
        Action::SidecarRestore { .. } => "sidecar_restore",
        Action::Discover { .. } => "discover",
        Action::ReconcilePaths { .. } => "reconcile_paths",
        Action::EvidenceExport { .. } => "evidence_export",
    }
    .into()
}
fn tagged(kind: &str, value: &impl Serialize) -> serde_json::Value {
    serde_json::json!({"kind":kind,"value":value})
}
fn terminal(
    attempt: &str,
    digest: &str,
    kind: &str,
    epoch: u64,
    result: serde_json::Value,
) -> Operation {
    Operation {
        id: uuid::Uuid::new_v4().to_string(),
        attempt: attempt.into(),
        request_digest: digest.into(),
        epoch: U64(epoch),
        kind: kind.into(),
        phase: "complete".into(),
        stage: "draining".into(),
        cancel_requested: false,
        progress: U64(1),
        result: Some(result),
        error: None,
    }
}
fn checked_plan(
    catalog: &Catalog,
    operation: &str,
    digest: &str,
) -> Result<(
    crate::catalog_metadata_write::Owner,
    i64,
    i64,
    crate::metadata_export::ExportPlan,
    Option<crate::metadata_export::ExportReceipt>,
    String,
)> {
    digest64(digest)?;
    let value = catalog
        .metadata_export_plan(operation)
        .map_err(native)?
        .ok_or_else(|| error(ErrorCode::InvalidRequest, "metadata sidecar plan not found"))?;
    let actual = &value.5;
    if actual != digest {
        return Err(error(
            ErrorCode::StaleSession,
            "sidecar plan authority changed",
        ));
    }
    Ok(value)
}
fn plan_json(catalog: &Catalog, operation: &str) -> AnyResult<Option<serde_json::Value>> {
    catalog
        .metadata_export_plan(operation)?
        .map(|(owner, revision, base_model, plan, receipt, authority)| {
            Ok(sidecar_json(
                owner, revision, base_model, plan, receipt, authority, 0,
            ))
        })
        .transpose()
}
fn sidecar_json(
    owner: crate::catalog_metadata_write::Owner,
    revision: i64,
    base_model: i64,
    plan: crate::metadata_export::ExportPlan,
    receipt: Option<crate::metadata_export::ExportReceipt>,
    authority: String,
    rowid: i64,
) -> serde_json::Value {
    serde_json::json!({"row":I64(rowid),"operation":plan.operation,"version":U64(plan.version as u64),"owner":owner,"revision":I64(revision),"base_model":I64(base_model),"destination":NativePath::from_path(&plan.destination),"expected":plan.expected,"payload_bytes":U64(plan.payload_bytes),"payload_digest":plan.payload_digest,"authority_blake3":authority,"current":true,"receipt":receipt})
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    #[test]
    fn write_admission_is_cancelable_before_actor_handshake() -> AnyResult<()> {
        let temp = tempfile::tempdir()?;
        let mut catalog = Catalog::open(temp.path().join("catalog"))?;
        let mut coordinator = Coordinator::default();
        let attempt = uuid::Uuid::new_v4().to_string();
        let response = coordinator.execute(
            "catalog",
            &mut catalog,
            Request::Start {
                attempt,
                action: Action::Resolve {
                    identity: ImageIdentity {
                        image_id: "missing".into(),
                        key: VariantKey::master("missing"),
                        metadata_revision: I64(0),
                        pixel_generation: I64(0),
                        shared_source_epoch: I64(0),
                        physical_generation: I64(0),
                    },
                    field: "rating".into(),
                    model: I64(1),
                },
            },
            &Limits::default(),
            &Cancellation::default(),
        )?;
        let Response::Admitted(admitted) = response else {
            unreachable!()
        };
        let canceled = coordinator.execute(
            "catalog",
            &mut catalog,
            Request::Cancel {
                operation: admitted.operation.clone(),
                epoch: admitted.epoch,
            },
            &Limits::default(),
            &Cancellation::default(),
        )?;
        let Response::Status(status) = canceled else {
            unreachable!()
        };
        assert!(status.operation.unwrap().cancel_requested);
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let Response::Status(status) = coordinator.execute(
                "catalog",
                &mut catalog,
                Request::Status {
                    operation: Some(admitted.operation.clone()),
                },
                &Limits::default(),
                &Cancellation::default(),
            )?
            else {
                unreachable!()
            };
            if status
                .operation
                .as_ref()
                .is_some_and(|operation| operation.phase == "canceled")
            {
                coordinator.close(&catalog)?;
                return Ok(());
            }
        }
        anyhow::bail!("metadata cancellation did not settle")
    }
}
