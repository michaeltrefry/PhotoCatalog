use crate::{
    catalog_edits::VariantKey, edit::Recipe, organization::Flag, storage_volume::NativePath,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

macro_rules! decimal {
    ($name:ident,$ty:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(pub $ty);
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = String::deserialize(d)?;
                let value = text.parse::<$ty>().map_err(D::Error::custom)?;
                if value.to_string() != text {
                    return Err(D::Error::custom("expected canonical decimal string"));
                }
                Ok(Self(value))
            }
        }
    };
}
decimal!(I64, i64);
decimal!(U64, u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "command",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    OpenExisting {
        path: NativePath,
    },
    Create {
        path: NativePath,
    },
    Status,
    Close {
        catalog: String,
    },
    Folders {
        catalog: String,
        parent: Option<I64>,
        after: I64,
        limit: u16,
    },
    Images {
        catalog: String,
        folder: Option<I64>,
        recursive: bool,
        text: Option<String>,
        cursor: Option<String>,
        limit: u16,
    },
    Image {
        catalog: String,
        key: VariantKey,
    },
    Variant {
        catalog: String,
        key: VariantKey,
    },
    Variants {
        catalog: String,
        asset_id: String,
        after: I64,
        limit: u16,
    },
    CreateVariant {
        catalog: String,
        key: VariantKey,
        expected_revision: I64,
        label: String,
    },
    SaveRecipe {
        catalog: String,
        key: VariantKey,
        expected_revision: I64,
        recipe: Recipe,
    },
    Undo {
        catalog: String,
        key: VariantKey,
        expected_revision: I64,
    },
    Redo {
        catalog: String,
        key: VariantKey,
        expected_revision: I64,
    },
    History {
        catalog: String,
        key: VariantKey,
        after: I64,
        limit: u16,
    },
    Cull {
        catalog: String,
        key: VariantKey,
        expected_revision: I64,
        operation: CullOperation,
    },
    Preview {
        catalog: String,
        key: VariantKey,
        tier: PreviewTier,
        interactive: bool,
        viewport: String,
        generation: U64,
        foreground: bool,
    },
    PreviewStatus {
        catalog: String,
        ticket: String,
    },
    ReleaseViewport {
        catalog: String,
        viewport: String,
        generation: U64,
    },
    CancelPreview {
        catalog: String,
        ticket: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CullOperation {
    Rating(u8),
    Flag(Flag),
    Label(String),
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewTier {
    Thumbnail,
    Large,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Closed,
    Opening,
    Indexing,
    Ready,
    Closing,
    Failed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub phase: Phase,
    pub catalog: Option<String>,
    pub jobs_held: bool,
    pub pending_commands: u32,
    pub active_previews: u32,
    pub cancel_requested: bool,
    pub message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: I64,
    pub parent: Option<I64>,
    pub locator: NativePath,
    pub name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridImage {
    pub image_id: String,
    pub key: VariantKey,
    pub sequence: I64,
    pub metadata_revision: I64,
    pub metadata_pending: bool,
    pub state: String,
    pub filename: String,
    pub rating: Option<I64>,
    pub flag: String,
    pub label: String,
    pub conflicts: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variant {
    pub key: VariantKey,
    pub label: String,
    pub revision: I64,
    pub recipe: Recipe,
    pub recipe_digest: String,
    pub can_undo: bool,
    pub can_redo: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub revision: I64,
    pub kind: String,
    pub recipe: Recipe,
    pub recipe_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewState {
    Queued,
    Ready,
    Stale,
    NeedsResources,
    Unavailable,
    Failed,
    CancelRequested,
    Canceled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewStatus {
    pub ticket: String,
    pub key: VariantKey,
    pub revision: I64,
    pub recipe_digest: String,
    pub viewport: String,
    pub generation: U64,
    pub state: PreviewState,
    pub message: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Status(Status),
    Folders {
        rows: Vec<Folder>,
        next: Option<I64>,
    },
    Images {
        rows: Vec<GridImage>,
        next: Option<String>,
        has_more: bool,
        page_complete: bool,
        scanned: u32,
    },
    Image(GridImage),
    Variant(Variant),
    Variants {
        rows: Vec<(I64, Variant)>,
        next: Option<I64>,
    },
    History {
        rows: Vec<HistoryEntry>,
        next: Option<I64>,
    },
    Culled {
        metadata_revision: I64,
    },
    Preview(PreviewStatus),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    Busy,
    Canceled,
    Superseded,
    StaleSession,
    Native,
    ResourceLimit,
    Closed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeError {
    pub code: ErrorCode,
    pub message: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Ok { value: Response },
    Error { error: BridgeError },
}
impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for BridgeError {}
