//! Resumable Lightroom migration into the catalog. Original files remain external.

pub mod evidence;

pub(crate) fn install(db: &rusqlite::Connection) -> anyhow::Result<()> {
    evidence::install(db)
}
