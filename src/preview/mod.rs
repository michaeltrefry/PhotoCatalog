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
pub use memory::{ByteBudget, ByteReservation, DecodedCache, RetainedPixels};
pub use scheduler::{
    Completion, Consumer, PreviewScheduler, Priority, SchedulerLimits, SchedulerUsage, WorkLease,
    WorkerOutcome,
};
pub use store::{
    CachedPreview, Layout, PreviewKey, PreviewStore, Publication, StoreConfig, StoreUsage, Tier,
};

mod worker;
pub use worker::{
    ProducedPreview, RenderWork, RenderedPreviewBatch, WorkerProcess, recover_worker_staging,
    worker_main,
};
