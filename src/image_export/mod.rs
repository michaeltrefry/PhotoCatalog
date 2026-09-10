//! Exact rendered derivatives. Publication, overwrite policy and source metadata
//! resolution belong to the caller; this module writes only its staging sink.
mod encode;
mod metadata;
mod sink;
mod specification;
pub use encode::{EncodingReport, encode_export};
pub use metadata::{Rational, ResolvedExportMetadata, SafeExif};
pub use sink::BoundedSeekWriter;
pub use specification::*;
#[cfg(test)]
mod tests;
