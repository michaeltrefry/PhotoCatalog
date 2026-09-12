//! Resumable Lightroom migration into the catalog. Original files remain external.

pub mod evidence;
pub mod originals;
pub mod retention;

pub(crate) fn install(db: &rusqlite::Connection) -> anyhow::Result<()> {
    evidence::install(db)?;
    originals::install(db)?;
    retention::install(db)
}
