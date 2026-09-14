//! Closed-roster source process. The destination executor consumes typed, bounded
//! immutable values; it never receives a source File or SQLite connection.
mod artifact_factory;
mod authority_json;
mod commit;
mod owner;
mod proxy;
pub(crate) mod relay;
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

/// Managed G-owned dispatch pins the allowed Source role before any input
/// authority is opened. The original direct mode remains a separate entrypoint.
pub fn managed_source_reader_main(raw: bool) -> anyhow::Result<()> {
    owner::serve_mode(
        std::io::stdin(),
        std::io::stdout().lock(),
        Some(if raw {
            relay::Kind::Raw
        } else {
            relay::Kind::Sql
        }),
    )
}
