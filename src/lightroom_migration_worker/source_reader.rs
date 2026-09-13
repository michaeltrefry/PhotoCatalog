//! Closed-roster source process. The destination executor consumes typed, bounded
//! immutable values; it never receives a source File or SQLite connection.
mod artifact_factory;
mod commit;
mod owner;
mod proxy;
mod transport;
mod wire;

pub(crate) use artifact_factory::RemoteArtifacts;
pub(crate) use commit::CommitHealth;
pub(crate) use proxy::{Health, RawReader, SqlReader};

/// Private worker mode on the configured installed executable. Do not initialize
/// the GUI, a destination Catalog, or file-based logging before dispatching it.
pub fn source_reader_main() -> anyhow::Result<()> {
    owner::serve(std::io::stdin(), std::io::stdout().lock())
}
