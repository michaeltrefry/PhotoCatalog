//! Owned Lightroom migration helper and source-bound desktop admission.
//! No API in this module infers approval, scans originals, or resumes on open.
pub mod authority;
pub mod identity;

pub(crate) mod lease;
pub mod protocol;

pub(crate) mod input;
pub(crate) mod process;
pub(crate) mod supervisor;

pub(crate) mod source_reader;

pub use source_reader::source_reader_main;
