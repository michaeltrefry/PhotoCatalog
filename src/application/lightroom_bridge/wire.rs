use super::*;
use serde::{Deserialize, Serialize};
macro_rules! limits {
    ($name:ident,$core:ty,[$($field:ident),+]) => {
        #[derive(Clone,Debug,Serialize,Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: U64),+ }
        impl From<$core> for $name { fn from(v:$core)->Self { Self { $($field:U64(v.$field as u64)),+ } } }
        impl TryFrom<$name> for $core { type Error=anyhow::Error; fn try_from(v:$name)->Result<Self> { Ok(Self { $($field:v.$field.0.try_into()?),+ }) } }
    }
}
limits!(
    WorkbenchLimits,
    lw::Limits,
    [
        request_bytes,
        result_bytes,
        page_bytes,
        row_bytes,
        native_path_units,
        vm_steps,
        deadline_ms
    ]
);
limits!(
    InspectionLimits,
    crate::lightroom::Limits,
    [
        max_files,
        max_depth,
        max_file_bytes,
        max_total_bytes,
        max_cell_bytes
    ]
);
limits!(
    SelectionLimits,
    crate::lightroom::selection::SelectionLimits,
    [
        review_bytes,
        row_bytes,
        page_bytes,
        native_path_units,
        snapshot_bytes,
        vm_steps,
        deadline_ms
    ]
);
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub workbench: String,
    pub generation: String,
    pub operation: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InputPurpose {
    Inventory,
    SelectionRequest,
    Approval,
    ApprovalDraft,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Action {
    Discover {
        root: NativePath,
        limits: InspectionLimits,
    },
    Capture {
        source: NativePath,
        output: NativePath,
        include_auxiliary: bool,
        closed_application_evidence: Option<String>,
        limits: InspectionLimits,
    },
    RegisterInventory {
        input: String,
    },
    AddCapture {
        directory: NativePath,
    },
    Resume {
        revision: String,
        max_rows: U64,
    },
    InspectOriginals {
        revision: String,
        limit: U64,
        inspection: lw::OriginalInspection,
    },
    AssignFamily {
        revision: String,
        family: String,
        reason: String,
    },
    Choose {
        family: String,
        revision: String,
        expected_evidence: String,
        reason: String,
    },
    PrepareSelection {
        input: String,
        limits: SelectionLimits,
    },
    Seal {
        review_token: String,
        approval_blake3: String,
        input: String,
        output: NativePath,
    },
    ApprovalDocuments {
        input: String,
        review_token: String,
    },
    ReleaseReview {},
}
/// All physical/tuple cursors already use exact decimal wrappers in the owner.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Query {
    CaptureManifest {
        directory: NativePath,
    },
    Rows {
        revision: String,
        table: Option<String>,
        after: I64,
        limit: U64,
    },
    Report {
        revision: String,
    },
    Paths {
        revision: String,
        after: I64,
        limit: U64,
    },
    Issues {
        revision: String,
        after: I64,
        limit: U64,
    },
    Packets {
        revision: String,
        after: I64,
        limit: U64,
    },
    PacketBytes {
        revision: String,
        sequence: I64,
        decoded: bool,
        offset: I64,
        limit: U64,
    },
    MetadataConflicts {
        revision: String,
        after: I64,
        limit: U64,
    },
    GlobalIdConflicts {
        left: String,
        right: String,
        after_left: String,
        after_right: String,
        limit: U64,
    },
    PathCollisions {
        left: String,
        right: String,
        after_left: I64,
        after_right: I64,
        limit: U64,
    },
    Families {},
    SelectionSummary {},
    SelectionSources {
        review_token: String,
        revision: String,
        after: I64,
        limit: U64,
    },
    SelectionPreparation {
        review_token: String,
        document: crate::lightroom::selection::PreparationDocument,
        offset: U64,
        limit: U64,
    },
    SelectionPage {
        review_token: String,
        collection: lw::ReviewCollection,
        after: U64,
        limit: U64,
    },
}
impl From<lw::Query> for Query {
    fn from(q: lw::Query) -> Self {
        match q {
            lw::Query::CaptureManifest { directory } => Self::CaptureManifest { directory },
            lw::Query::Rows {
                revision,
                table,
                after,
                limit,
            } => Self::Rows {
                revision,
                table,
                after,
                limit,
            },
            lw::Query::Report { revision } => Self::Report { revision },
            lw::Query::Paths {
                revision,
                after,
                limit,
            } => Self::Paths {
                revision,
                after,
                limit,
            },
            lw::Query::Issues {
                revision,
                after,
                limit,
            } => Self::Issues {
                revision,
                after,
                limit,
            },
            lw::Query::Packets {
                revision,
                after,
                limit,
            } => Self::Packets {
                revision,
                after,
                limit,
            },
            lw::Query::PacketBytes {
                revision,
                sequence,
                decoded,
                offset,
                limit,
            } => Self::PacketBytes {
                revision,
                sequence,
                decoded,
                offset,
                limit,
            },
            lw::Query::MetadataConflicts {
                revision,
                after,
                limit,
            } => Self::MetadataConflicts {
                revision,
                after,
                limit,
            },
            lw::Query::GlobalIdConflicts {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => Self::GlobalIdConflicts {
                left,
                right,
                after_left,
                after_right,
                limit,
            },
            lw::Query::PathCollisions {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => Self::PathCollisions {
                left,
                right,
                after_left,
                after_right,
                limit,
            },
            lw::Query::Families => Self::Families {},
            lw::Query::SelectionSummary => Self::SelectionSummary {},
            lw::Query::SelectionSources {
                review_token,
                revision,
                after,
                limit,
            } => Self::SelectionSources {
                review_token,
                revision,
                after,
                limit,
            },
            lw::Query::SelectionPreparation {
                review_token,
                document,
                offset,
                limit,
            } => Self::SelectionPreparation {
                review_token,
                document,
                offset,
                limit,
            },
            lw::Query::SelectionPage {
                review_token,
                collection,
                after,
                limit,
            } => Self::SelectionPage {
                review_token,
                collection,
                after,
                limit,
            },
        }
    }
}
impl From<Query> for lw::Query {
    fn from(q: Query) -> Self {
        match q {
            Query::CaptureManifest { directory } => Self::CaptureManifest { directory },
            Query::Rows {
                revision,
                table,
                after,
                limit,
            } => Self::Rows {
                revision,
                table,
                after,
                limit,
            },
            Query::Report { revision } => Self::Report { revision },
            Query::Paths {
                revision,
                after,
                limit,
            } => Self::Paths {
                revision,
                after,
                limit,
            },
            Query::Issues {
                revision,
                after,
                limit,
            } => Self::Issues {
                revision,
                after,
                limit,
            },
            Query::Packets {
                revision,
                after,
                limit,
            } => Self::Packets {
                revision,
                after,
                limit,
            },
            Query::PacketBytes {
                revision,
                sequence,
                decoded,
                offset,
                limit,
            } => Self::PacketBytes {
                revision,
                sequence,
                decoded,
                offset,
                limit,
            },
            Query::MetadataConflicts {
                revision,
                after,
                limit,
            } => Self::MetadataConflicts {
                revision,
                after,
                limit,
            },
            Query::GlobalIdConflicts {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => Self::GlobalIdConflicts {
                left,
                right,
                after_left,
                after_right,
                limit,
            },
            Query::PathCollisions {
                left,
                right,
                after_left,
                after_right,
                limit,
            } => Self::PathCollisions {
                left,
                right,
                after_left,
                after_right,
                limit,
            },
            Query::Families {} => Self::Families,
            Query::SelectionSummary {} => Self::SelectionSummary,
            Query::SelectionSources {
                review_token,
                revision,
                after,
                limit,
            } => Self::SelectionSources {
                review_token,
                revision,
                after,
                limit,
            },
            Query::SelectionPreparation {
                review_token,
                document,
                offset,
                limit,
            } => Self::SelectionPreparation {
                review_token,
                document,
                offset,
                limit,
            },
            Query::SelectionPage {
                review_token,
                collection,
                after,
                limit,
            } => Self::SelectionPage {
                review_token,
                collection,
                after,
                limit,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Request {
    SealedDocument {
        request: crate::filesystem_worker::wire::LightroomSealedRead,
    },
    ArtifactPreparation {
        request: crate::filesystem_worker::wire::LightroomArtifactPreparation,
    },
    Options {},
    Open {
        attempt: String,
        root: NativePath,
        mode: lw::OpenMode,
        capture_staging: NativePath,
        limits: WorkbenchLimits,
    },
    Status {
        workbench: Option<String>,
        attempt: Option<String>,
    },
    Action {
        guard: Guard,
        action: Action,
    },
    Read {
        guard: Guard,
        query: Query,
    },
    Cancel {
        guard: Guard,
    },
    Close {
        workbench: String,
    },
    Result {
        guard: Guard,
        token: String,
        offset: U64,
        limit: U64,
    },
    InputBegin {
        guard: Guard,
        purpose: InputPurpose,
        total_bytes: U64,
        expected_blake3: Option<String>,
    },
    InputAppend {
        guard: Guard,
        input: String,
        offset: U64,
        fragment: String,
    },
    InputFinish {
        guard: Guard,
        input: String,
    },
    InputDiscard {
        guard: Guard,
        input: String,
    },
    InputStatus {
        guard: Guard,
        input: Option<String>,
    },
}
impl Request {
    pub(crate) fn direct(&self) -> bool {
        matches!(
            self,
            Self::Options {}
                | Self::SealedDocument { .. }
                | Self::ArtifactPreparation { .. }
                | Self::Status { .. }
                | Self::Cancel { .. }
                | Self::Close { .. }
                | Self::Result { .. }
                | Self::InputStatus { .. }
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Options {
    pub envelope_bytes: U64,
    pub chunk_bytes: U64,
    pub input_slots: U64,
    pub input_owned_factor: U64,
    pub minimum_nonfinal_chunk_bytes: U64,
    pub selection_preparation_chunk_bytes: U64,
    pub selection_preparation_page_rows: U64,
    pub workbench: WorkbenchLimits,
    pub inspection: InspectionLimits,
    pub selection: SelectionLimits,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub attempt: String,
    pub workbench: String,
    pub generation: String,
    pub operation: String,
    pub phase: lw::Phase,
    pub initialized: bool,
    pub closed: bool,
    pub root: NativePath,
    pub limits: WorkbenchLimits,
    pub processed: U64,
    pub result_token: Option<String>,
    pub result_bytes: U64,
    pub review_token: Option<String>,
    pub capture_pid: Option<U64>,
    pub capture_staging: Option<NativePath>,
    pub error: Option<String>,
}
impl Status {
    pub(super) fn new(attempt: String, s: lw::Status) -> Self {
        Self {
            attempt,
            workbench: s.workbench,
            generation: s.generation,
            operation: s.operation,
            phase: s.phase,
            initialized: s.initialized,
            closed: s.closed,
            root: s.root,
            limits: s.limits.into(),
            processed: s.processed,
            result_token: s.result_token,
            result_bytes: s.result_bytes,
            review_token: s.review_token,
            capture_pid: s.capture_pid.map(|v| U64(v.into())),
            capture_staging: s.capture_staging,
            error: s.error,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultPage {
    pub attempt: String,
    #[serde(flatten)]
    pub page: lw::ResultPage,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputStatus {
    pub attempt: String,
    pub guard: Guard,
    pub input: String,
    pub purpose: InputPurpose,
    pub total_bytes: U64,
    pub received_bytes: U64,
    pub blake3: Option<String>,
    pub expected_blake3: Option<String>,
    pub complete: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum Response {
    Options(Options),
    Status(Option<Status>),
    Result(ResultPage),
    Input(Option<InputStatus>),
    SealedDocument(Option<crate::filesystem_worker::wire::LightroomSealedDocumentPage>),
    ArtifactPreparation(Option<crate::filesystem_worker::wire::LightroomArtifactPreparationReply>),
}
