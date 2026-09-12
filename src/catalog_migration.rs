//! Resumable Lightroom migration into the catalog. Original files remain external.

pub mod artifacts;
pub mod current_repair;
pub mod evidence;
pub mod file_metadata;
pub mod history;
pub mod images;
pub mod import_artifacts;
pub mod importer;
pub mod lookup;
pub mod metadata;
pub mod organization;
mod organization_walk;
pub mod originals;
pub mod reconciliation;
pub mod retention;
pub mod supplements;
mod walk;

#[cfg(test)]
mod importer_tests;

pub(crate) fn install(db: &rusqlite::Connection) -> anyhow::Result<()> {
    evidence::install(db)?;
    originals::install(db)?;
    retention::install(db)?;
    lookup::install(db)?;
    artifacts::install(db)?;
    organization::install(db)?;
    images::install(db)?;
    metadata::install(db)?;
    file_metadata::install(db)?;
    importer::install(db)?;
    reconciliation::install(db)
}
