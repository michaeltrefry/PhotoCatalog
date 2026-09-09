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
    ByteBudget, ByteReservation, DecodedBudgetExceeded, DecodedCache, RetainedPixels,
};
pub use scheduler::{
    Completion, Consumer, PreviewScheduler, Priority, SchedulerLimits, SchedulerUsage, WorkLease,
    WorkerOutcome,
};
pub use store::{
    CacheQuotaExceeded, CachedPreview, Layout, PreviewKey, PreviewStore, Publication,
    RelocationProgress, RenderRecord, StoreConfig, StoreUsage, Tier,
};

mod worker;
pub use worker::{
    ProducedPreview, RenderWork, RenderedPreviewBatch, WorkerFailure, WorkerProcess,
    recover_worker_staging, worker_main,
};

mod service;
pub use service::{
    EncodedPreview, JobState, JobView, PreviewPolicy, PreviewService, PreviewView,
    ServiceCompletion, ServiceEvent, ServiceLimits, TierPolicy,
};

mod config;
pub use config::PreviewConfiguration;
