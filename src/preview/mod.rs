//! Persistent browse previews. These RGB8 surfaces never replace float editor input.
mod codec;
mod metrics;
pub use codec::PREPARATION_VERSION;
pub use codec::{Codec, CodecSettings, PreparedRgb, decode, encode, prepare, versions};
pub use metrics::{QualityMetrics, quality_metrics};
