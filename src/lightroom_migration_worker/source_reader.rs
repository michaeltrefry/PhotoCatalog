//! Closed-roster source process. The destination executor consumes typed, bounded
//! immutable values; it never receives a source File or SQLite connection.
mod artifact_factory;
mod authority_json;
mod capture_source;
pub mod capture_wire;
mod commit;
mod owner;
mod proxy;
pub(crate) mod relay;
mod transport;
mod wire;

pub(crate) use artifact_factory::RemoteArtifacts;
pub use capture_wire::{Authority as CaptureSqlAuthority, MEMBER as CAPTURE_SQL_MEMBER};
pub(crate) use commit::CommitHealth;
#[allow(unused_imports)]
pub(crate) use proxy::{CaptureSqlReader, Health, RawReader, SqlReader};

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

pub fn managed_capture_sql_reader_main() -> anyhow::Result<()> {
    owner::serve_mode(
        std::io::stdin(),
        std::io::stdout().lock(),
        Some(relay::Kind::CaptureSql),
    )
}

#[cfg(test)]
pub(crate) fn test_source_reader_main(
    raw: bool,
    output: impl std::io::Write,
) -> anyhow::Result<()> {
    owner::serve_mode(
        std::io::stdin(),
        output,
        Some(if raw {
            relay::Kind::Raw
        } else {
            relay::Kind::Sql
        }),
    )
}
