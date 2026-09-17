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
pub(crate) use proxy::reader_metadata_layouts;
#[allow(unused_imports)]
pub(crate) use proxy::{CaptureSqlReader, Health, RawReader, SqlReader};

pub(crate) const fn managed_result_bytes() -> usize {
    transport::RESULT_BYTES
}

/// Complete local pool retained by one managed Workbench Source router. This
/// uses the public accepted maxima of all three fixed Source kinds and the same
/// phase formula used by RelayClient at runtime. It is an admission allowance,
/// not a new wire or document limit.
fn managed_requirement(core: usize, include_broker: bool) -> anyhow::Result<usize> {
    use crate::lightroom::{MANIFEST_BYTES, PAGE_BYTES, migration_source::ReadLimits};
    use crate::lightroom_migration_worker::memory::layout::{
        add, mul, seal_dynamic, seal_validation,
    };
    use relay::{COUNT, Kind};
    use std::mem::size_of;
    use transport::{AUTHORITY_BYTES, RESULT_BYTES, Read};
    use wire::{Budget, Expected, Query, Value};

    let sql_limits = ReadLimits {
        page_bytes: PAGE_BYTES,
        inline_bytes: PAGE_BYTES / 4,
        chunk_bytes: 1024 * 1024,
        vm_steps: 1_000_000_000,
        deadline_ms: 120_000,
        open_deadline_ms: 3_600_000,
    };
    sql_limits.validate()?;
    let capture_limits = capture_wire::Limits {
        open_deadline_ms: crate::application::U64(120_000),
        total_deadline_ms: crate::application::U64(3_600_000),
        vm_steps: crate::application::U64(u32::MAX as u64),
        schema_objects: crate::application::U64(capture_wire::MAX_SCHEMA_OBJECTS as u64),
        schema_bytes: crate::application::U64(PAGE_BYTES as u64),
        page_bytes: crate::application::U64(PAGE_BYTES as u64),
        max_cell_bytes: crate::application::U64(64 * 1024 * 1024),
        result_bytes: crate::application::U64(RESULT_BYTES as u64),
        inline_bytes: crate::application::U64(PAGE_BYTES as u64),
        chunk_bytes: crate::application::U64(capture_wire::MAX_CHUNK_BYTES as u64),
        max_rows: crate::application::U64(capture_wire::MAX_ROWS as u64),
    };
    capture_limits.validate()?;

    let sql_opening_graph = add(
        add(
            MANIFEST_BYTES,
            add(
                mul(
                    16_384,
                    size_of::<crate::lightroom::migration_source::SelectedCapture>()
                        .max(size_of::<String>()),
                )?,
                mul(
                    16_384,
                    size_of::<crate::lightroom::migration_source::SupplementPin>(),
                )?,
            )?,
        )?,
        add(seal_dynamic()?, seal_validation()?)?,
    )?;
    let raw_opening_graph = mul(3, 64 * 1024)?;
    let capture_opening_graph = capture_source::graph_allocation(capture_limits)?;
    let authority_copies = mul(3, AUTHORITY_BYTES)?;
    let mut opening = [0usize; COUNT];
    opening[Kind::Sql.index()] = add(sql_opening_graph, authority_copies)?;
    opening[Kind::Raw.index()] = add(raw_opening_graph, authority_copies)?;
    opening[Kind::CaptureSql.index()] = add(capture_opening_graph, authority_copies)?;

    let binding = "0";
    let sql_queries = [
        Query::CaptureManifest {
            revision: String::new(),
        },
        Query::StableSource {
            revision: String::new(),
            source_id: String::new(),
        },
        Query::OriginPacketRoster {
            revision: String::new(),
            source_id: String::new(),
            origin: String::new(),
        },
        Query::Page {
            revision: String::new(),
            collection: crate::lightroom::migration_source::Collection::References,
            after: None,
            limit: crate::application::U64(1000),
        },
        Query::ReadChunk {
            reference: crate::lightroom::migration_source::ByteRef {
                seal: String::new(),
                revision: String::new(),
                collection: crate::lightroom::migration_source::Collection::Rows,
                rowid: 1,
                field: String::new(),
                bytes: 1,
                text: false,
            },
            offset: crate::application::U64(0),
            limit: crate::application::U64(sql_limits.chunk_bytes as u64),
        },
        Query::Count {
            revision: String::new(),
            collection: crate::lightroom::migration_source::Collection::Rows,
        },
        Query::Resolve {
            revision: String::new(),
            source_id: String::new(),
            field: String::new(),
            target_table: String::new(),
        },
        Query::ImageLinks {
            revision: String::new(),
            source_id: String::new(),
        },
    ];
    let mut sql_producer = Value::sql_opening_allocation(sql_limits)?;
    let mut transient = 0usize;
    let mut graph = 0usize;
    for query in &sql_queries {
        let read = Read::Sql(query.clone());
        let expected = Expected {
            read: &read,
            budget: Budget::Sql(sql_limits),
            binding,
        };
        sql_producer = sql_producer.max(Value::producer_allocation(expected)?);
        let expected = Expected {
            read: &read,
            budget: Budget::Sql(sql_limits),
            binding,
        };
        let (candidate_transient, candidate_graph) = Value::allocation(RESULT_BYTES, expected)?;
        transient = transient.max(candidate_transient);
        graph = graph.max(candidate_graph);
    }
    let raw_read = Read::ArtifactChunk {
        offset: crate::application::U64(0),
    };
    let raw_expected = Expected {
        read: &raw_read,
        budget: Budget::Raw(1024 * 1024),
        binding,
    };
    let raw_producer = Value::producer_allocation(raw_expected)?;
    let raw_expected = Expected {
        read: &raw_read,
        budget: Budget::Raw(1024 * 1024),
        binding,
    };
    let (raw_transient, raw_graph) = Value::allocation(RESULT_BYTES, raw_expected)?;
    transient = transient.max(raw_transient);
    graph = graph.max(raw_graph);

    let capture_read = Read::CaptureSql(capture_wire::Query::TableRows {
        table_handle: String::new(),
        cursor: None,
        limit: crate::application::U64(capture_wire::MAX_ROWS as u64),
    });
    let capture_expected = Expected {
        read: &capture_read,
        budget: Budget::Capture(capture_limits),
        binding,
    };
    let capture_producer = Value::producer_allocation(capture_expected)?;
    let capture_expected = Expected {
        read: &capture_read,
        budget: Budget::Capture(capture_limits),
        binding,
    };
    let (capture_transient, capture_graph) = Value::allocation(RESULT_BYTES, capture_expected)?;
    transient = transient.max(capture_transient);
    graph = graph.max(capture_graph);

    let mut producer = [0usize; COUNT];
    producer[Kind::Sql.index()] = sql_producer;
    producer[Kind::Raw.index()] = raw_producer;
    producer[Kind::CaptureSql.index()] = capture_producer;

    // Each child retains its own checked opening grant in addition to the
    // parent-side authority/result graph. Sql and Raw use the shared
    // closed-roster path bound; CaptureSql requests its exact schema/result and
    // installed progress-callback opening allowance.
    let path_opening = crate::lightroom::source::managed_opening_reservation_maximum()?;
    let child_opening = add(
        mul(2, path_opening)?,
        capture_source::opening_allocation(capture_limits)?,
    )?;
    let client =
        relay::client::managed_allocation_requirement(opening, producer, transient, graph, core)?;
    let owners = add(client, child_opening)?;
    if include_broker {
        add(relay::broker::Broker::allocation_backing()?, owners)
    } else {
        Ok(owners)
    }
}

pub(crate) fn managed_workbench_requirement() -> anyhow::Result<usize> {
    managed_requirement(0, true)
}

/// Migration's supervisor separately reserves the Broker and Process owners.
/// This term covers the LM-side client, every supported Source phase and both
/// child opening grants, including the operation's largest core-owned phase.
pub(crate) fn managed_migration_requirement(core: usize) -> anyhow::Result<usize> {
    managed_requirement(core, false)
}

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
