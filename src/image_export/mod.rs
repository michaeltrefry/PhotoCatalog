//! Exact rendered derivatives. Publication, overwrite policy and source metadata
//! resolution belong to the caller; this module writes only its staging sink.
mod sink;
mod specification;
mod metadata;
mod encode;
pub use sink::BoundedSeekWriter;
pub use specification::*;
pub use metadata::{ResolvedExportMetadata, SafeExif, Rational};
pub use encode::{encode_export, EncodingReport};
#[cfg(test)] mod tests;
