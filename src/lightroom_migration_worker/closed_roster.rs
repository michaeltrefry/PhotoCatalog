//! Shared terminal admission for immutable SQLite source roles.
//!
//! Capability-specific code must validate and open its one authorized main
//! object before calling this constructor. After this consumes the handles it
//! acquires the custom lock and permits no further pathname or SQLite opens.
use crate::lightroom::source::Source;
use anyhow::{Context, Result};
use rusqlite::Connection;

pub(crate) struct ClosedImmutableRoster {
    connection: Connection,
    source: Source,
    initial_data_version: i64,
}

impl ClosedImmutableRoster {
    pub(crate) fn finish(
        connection: Connection,
        mut source: Source,
        mut verify_companions: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        source.verify()?;
        verify_companions()?;
        crate::catalog_storage::verify_database_object(&connection, &source.file)
            .context("closed immutable roster opened object")?;
        source.lock(0x4000_0000, 512)?;
        source.verify()?;
        verify_companions()?;
        crate::catalog_storage::verify_database_object(&connection, &source.file)
            .context("locked immutable roster opened object")?;
        let initial_data_version =
            connection.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        Ok(Self {
            connection,
            source,
            initial_data_version,
        })
    }

    pub(crate) fn into_parts(self) -> (Connection, Source, i64) {
        (self.connection, self.source, self.initial_data_version)
    }
}
