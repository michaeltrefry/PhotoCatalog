use super::*;
use crate::image_export::{AlphaPolicy, OutputFormat, OutputSize};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U128(pub u128);
impl Serialize for U128 {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for U128 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let n = s.parse::<u128>().map_err(D::Error::custom)?;
        if n.to_string() != s {
            return Err(D::Error::custom("canonical unsigned decimal required"));
        }
        Ok(Self(n))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Profile {
    Srgb,
    LinearSrgb,
    Icc { token: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub size: OutputSize,
    pub format: OutputFormat,
    pub profile: Profile,
    pub alpha: AlphaPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasLimits {
    pub directories: U64,
    pub candidates: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub max_original_bytes: U64,
    pub max_payload_bytes: U64,
    pub alias_limits: AliasLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderLimits {
    pub max_pixels: U64,
    pub max_allocation_bytes: U64,
    pub max_live_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeLimits {
    pub max_encoded_bytes: U64,
    pub max_intermediate_pixels: U64,
    pub max_allocation_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeLimits {
    pub render: RenderLimits,
    pub max_metadata_bytes: U64,
    pub row_buffer_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhotoLimits {
    pub decode: DecodeLimits,
    pub render: RenderLimits,
    pub encode: EncodeLimits,
    pub max_encoded_extent: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLimits {
    pub worker_bytes: U64,
    pub working_bytes: U64,
    pub render: PhotoLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Options {
    pub budgets: Budgets,
    pub execution: ExecutionLimits,
    pub page_rows: U64,
    pub page_bytes: U64,
    pub path_bytes: U64,
    pub profile_bytes: U64,
    pub profile_tokens: U64,
    pub profile_total_bytes: U64,
    pub result_rows: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Metadata {
    Omit,
    Resolved {
        expected_revision: I64,
        base_model: Option<I64>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetKey {
    pub key: VariantKey,
    pub expected_revision: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub key: VariantKey,
    pub expected_revision: I64,
    pub destination: NativePath,
    pub overwrite: bool,
    pub metadata: Metadata,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Naming {
    pub prefix: String,
    pub suffix: String,
    pub variant_suffix: bool,
    pub sequence_start: Option<U64>,
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
    Profile {
        path: NativePath,
    },
    ProfileRelease {
        token: String,
    },
    Begin,
    Paths {
        limit: U64,
    },
    Destinations {
        directory: NativePath,
        targets: Vec<TargetKey>,
        format: OutputFormat,
        naming: Naming,
    },
    DestinationRows {
        token: String,
        after: U64,
        limit: U64,
    },
    ResultRelease {
        token: String,
    },
    Append {
        job: String,
        expected_total: I64,
        target: Target,
        output: Output,
        budgets: Option<Budgets>,
    },
    Seal {
        job: String,
        expected_total: I64,
    },
    Job {
        job: String,
    },
    Jobs {
        after: I64,
        limit: U64,
    },
    Items {
        job: String,
        after: I64,
        limit: U64,
    },
    Plan {
        job: String,
        sequence: I64,
    },
    PlanChunk {
        job: String,
        sequence: I64,
        authority: String,
        offset: U64,
        bytes: U64,
    },
    Run {
        job: String,
        limits: Option<ExecutionLimits>,
        max_items: U64,
        max_seconds: U64,
    },
    Status {
        operation: Option<String>,
    },
    Cancel {
        job: Option<String>,
        operation: Option<String>,
    },
    Yield {
        job: String,
        operation: String,
    },
    Recover {
        directories: U64,
        limits: Option<ExecutionLimits>,
    },
    RetrySeal {
        job: String,
        sequence: I64,
        authority: String,
    },
    Restore {
        job: String,
        sequence: I64,
        authority: String,
    },
}
impl Request {
    pub(in crate::application) fn read_only(&self) -> bool {
        matches!(
            self,
            Self::Options
                | Self::Job { .. }
                | Self::Jobs { .. }
                | Self::Items { .. }
                | Self::Plan { .. }
                | Self::PlanChunk { .. }
                | Self::DestinationRows { .. }
                | Self::Status { .. }
                | Self::ProfileRelease { .. }
                | Self::ResultRelease { .. }
        )
    }
    pub(super) fn job_id(&self) -> Option<&str> {
        match self {
            Self::Append { job, .. }
            | Self::Run { job, .. }
            | Self::RetrySeal { job, .. }
            | Self::Restore { job, .. } => Some(job),
            Self::Cancel { job: Some(job), .. } => Some(job),
            _ => None,
        }
    }
    pub(super) fn kind(&self) -> Option<&'static str> {
        Some(match self {
            Self::Profile { .. } => "profile",
            Self::Paths { .. } => "paths",
            Self::Destinations { .. } => "destinations",
            Self::Append { .. } => "append",
            Self::Run { .. } => "run",
            Self::Recover { .. } => "recover",
            Self::RetrySeal { .. } => "retry_seal",
            Self::Restore { .. } => "restore",
            Self::Cancel { .. } => "cancel",
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub sequence: I64,
    pub id: String,
    pub state: String,
    pub total: I64,
    pub completed: I64,
}
impl From<core::ExportJob> for Job {
    fn from(v: core::ExportJob) -> Self {
        Self {
            sequence: I64(v.sequence),
            id: v.id,
            state: v.state,
            total: I64(v.total),
            completed: I64(v.completed),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Name {
    pub filename: String,
    pub variant_label: String,
    pub available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRevision {
    pub bytes: U64,
    pub digest: String,
    pub modified_ns: U128,
    pub identity: (U64, U64),
}
impl From<crate::metadata_export::FileRevision> for FileRevision {
    fn from(v: crate::metadata_export::FileRevision) -> Self {
        Self {
            bytes: U64(v.bytes),
            digest: v.digest,
            modified_ns: U128(v.modified_ns),
            identity: (U64(v.identity.0), U64(v.identity.1)),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub state: String,
    pub destination: NativePath,
    pub recovery_directory: NativePath,
    pub captured_original: Option<NativePath>,
    pub detail: String,
}
impl From<crate::metadata_export::ExportReceipt> for Receipt {
    fn from(v: crate::metadata_export::ExportReceipt) -> Self {
        Self {
            state: format!("{:?}", v.state).to_ascii_lowercase(),
            destination: NativePath::from_path(&v.destination),
            recovery_directory: NativePath::from_path(&v.recovery_directory),
            captured_original: v.captured_original.map(|p| NativePath::from_path(&p)),
            detail: v.detail,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub sequence: I64,
    pub key: VariantKey,
    pub name: Name,
    pub destination: NativePath,
    pub state: String,
    pub attempt: Option<String>,
    pub authority: String,
    pub error: Option<String>,
    pub receipt: Option<Receipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageIdentity {
    pub image_id: String,
    pub key: VariantKey,
    pub metadata_revision: I64,
    pub pixel_generation: I64,
    pub shared_source_epoch: I64,
    pub physical_generation: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub asset_id: String,
    pub generation: I64,
    pub fingerprint: Option<String>,
    pub state: String,
    pub metadata_revision: I64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderIdentity {
    pub image_identity: Option<ImageIdentity>,
    pub source: SourceIdentity,
    pub key: VariantKey,
    pub revision: I64,
    pub recipe_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredProfile {
    Srgb,
    LinearSrgb,
    Icc {
        blob: String,
        bytes: U64,
        linear: Option<bool>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredOutput {
    pub size: OutputSize,
    pub format: OutputFormat,
    pub profile: StoredProfile,
    pub alpha: AlphaPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub digest: String,
    pub bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationSnapshot {
    pub version: u32,
    pub operation: String,
    pub destination: NativePath,
    pub expected: Option<FileRevision>,
    pub max_existing_bytes: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub job: String,
    pub sequence: I64,
    pub authority: String,
    pub plan_bytes: U64,
    pub version: u32,
    pub renderer_identity: String,
    pub name: Name,
    pub identity: RenderIdentity,
    pub original: NativePath,
    pub original_revision: FileRevision,
    pub output: StoredOutput,
    pub metadata: Metadata,
    pub xmp_blob: Option<Blob>,
    pub destination: DestinationSnapshot,
    pub budgets: Budgets,
    pub item: Item,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanChunk {
    pub job: String,
    pub sequence: I64,
    pub authority: String,
    pub offset: U64,
    pub next: Option<U64>,
    pub total_bytes: U64,
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileAdmission {
    pub token: String,
    pub name: String,
    pub bytes: U64,
    pub blake3: String,
    pub linear: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub projected: U64,
    pub pending: bool,
    pub unbound: U64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub target: TargetKey,
    pub name: Name,
    pub destination: Option<NativePath>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recovery {
    pub fenced: U64,
    pub complete: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ResultValue {
    Profile(ProfileAdmission),
    Paths(Paths),
    Destinations {
        token: String,
        total: U64,
    },
    Appended {
        job: Job,
        item: Box<Item>,
    },
    Job(Job),
    Recovery(Recovery),
    Receipt {
        job: Job,
        sequence: I64,
        authority: String,
        receipt: Receipt,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub kind: String,
    pub phase: String,
    pub job: Option<Job>,
    pub sequence: Option<I64>,
    pub stage: String,
    pub stream_bytes: Option<U64>,
    pub processed: U64,
    pub write_hold: bool,
    pub result: Option<Box<ResultValue>>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Response {
    Options(Options),
    Released {
        token: String,
    },
    Job(Job),
    Jobs {
        rows: Vec<Job>,
        next: Option<I64>,
    },
    Items {
        rows: Vec<Item>,
        next: Option<I64>,
    },
    Plan(Box<Plan>),
    PlanChunk(PlanChunk),
    Destinations {
        rows: Vec<Destination>,
        next: Option<U64>,
        total: U64,
    },
    Operation(Option<Operation>),
}
