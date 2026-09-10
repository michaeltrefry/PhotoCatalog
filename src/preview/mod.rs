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
pub use scheduler::{
    Completion, Consumer, PreviewScheduler, Priority, SchedulerLimits, SchedulerUsage, WorkLease,
    WorkerOutcome,
};
pub use store::{
    CacheQuotaExceeded, CachedPreview, EditInputProvenance, Layout, PreviewKey, PreviewStore,
    Publication, RelocationProgress, RenderRecord, StoreConfig, StoreUsage, Tier,
};

mod prepared_cache;
mod worker;
pub(crate) use worker::peak_resident_memory;
pub use worker::{
    EditWork, ProducedPreview, RenderWork, RenderedPreviewBatch, WorkerFailure, WorkerProcess,
    recover_worker_staging, worker_main,
};

mod service;
pub use service::{
    CacheReadMetrics, EncodedPreview, JobState, JobView, NativeLaunchPause, PreviewPolicy,
    PreviewService, PreviewView, ReadCompletion, ReadOutcome, ReadQueueUsage, ReadTicket,
    ServiceCompletion, ServiceEvent, ServiceLimits, TierPolicy, WorkerResourceMetrics,
};

mod config;
pub use config::PreviewConfiguration;
