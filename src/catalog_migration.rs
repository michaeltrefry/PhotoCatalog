//! Resumable Lightroom migration into the catalog. Original files remain external.

pub mod artifacts;
pub mod evidence;
pub mod images;
pub mod lookup;
pub mod metadata;
pub mod organization;
pub mod originals;
pub mod retention;

pub(crate) fn install(db: &rusqlite::Connection) -> anyhow::Result<()> {
    evidence::install(db)?;
    originals::install(db)?;
    retention::install(db)?;
    lookup::install(db)?;
    artifacts::install(db)?;
    organization::install(db)?;
    images::install(db)?;
    metadata::install(db)
}
