//! Persistent browse previews. These RGB8 surfaces never replace float editor input.
mod codec;
mod metrics;
pub use codec::PREPARATION_VERSION;
pub use codec::{
    Codec, CodecSettings, PreparedRgb, decode, encode, encoded_dimensions, prepare, versions,
};
pub use metrics::{QualityMetrics, quality_metrics};
mod identity;
mod memory;
mod scheduler;
mod store;
pub use identity::renderer_identity;
pub use memory::{
    ByteBudget, ByteReservation, DecodedBudgetExceeded, DecodedCache, EncodedBudgetExceeded,
    RetainedPixels,
};
pub(crate) use memory::{ByteLimit, budget_state_layout};
pub use scheduler::{
    Completion, Consumer, PreviewScheduler, Priority, SchedulerLimits, SchedulerUsage, WorkLease,
    WorkerOutcome,
};
pub use store::{
    CacheQuotaExceeded, CachedPreview, EditInputProvenance, Layout, PreviewKey, PreviewStore,
    Publication, RelocationProgress, RenderRecord, StoreConfig, StoreUsage, Tier,
};

pub(crate) mod prepared_cache;
pub(crate) mod stage_io;
mod worker;
pub(crate) use worker::peak_resident_memory;
pub use worker::{
    EditWork, ProducedPreview, RenderWork, RenderedPreviewBatch, WorkerFailure, WorkerProcess,
    recover_worker_staging, worker_main,
};
pub(crate) use worker::{managed_process::metadata_owner_layouts, receipt_metadata_layouts};

mod service;
pub(crate) use service::encoded_delivery;
pub(crate) use service::managed_read_metadata_root_layout;
pub use service::{
    CacheReadMetrics, EncodedPreview, HydrationRequest, JobState, JobView, NativeLaunchPause,
    NativeLaunchPermit, PreviewPolicy, PreviewService, PreviewView, ReadCompletion, ReadOutcome,
    ReadQueueUsage, ReadTicket, ServiceCompletion, ServiceEvent, ServiceLimits, TierPolicy,
    WorkerResourceMetrics,
};

mod config;
pub use config::PreviewConfiguration;

pub(crate) use store::custody::ManagedFiles as ManagedStoreFiles;
pub(crate) use store::{AdmittedStoreFiles, ManifestOrigin};

pub(crate) mod transport_task;
