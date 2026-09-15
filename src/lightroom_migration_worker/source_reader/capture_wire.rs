//! Closed CaptureSql vocabulary. Requests carry only opaque handles and typed
//! cursors; paths, SQL, identifiers, and sort expressions never cross this seam.
use crate::{
    application::{I64, U64},
    lightroom::{PAGE_BYTES, plan::Cell, source::Revision},
    lightroom_migration_worker::identity::FileKey,
    storage_volume::NativePath,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const MEMBER: &str = "logical.sqlite3";
pub(crate) const MAX_SCHEMA_OBJECTS: usize = 4096;
pub(crate) const MAX_ROWS: usize = 100;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub open_deadline_ms: U64,
    pub total_deadline_ms: U64,
    pub vm_steps: U64,
    pub schema_objects: U64,
    pub schema_bytes: U64,
    pub page_bytes: U64,
    pub max_cell_bytes: U64,
    pub result_bytes: U64,
    pub inline_bytes: U64,
    pub chunk_bytes: U64,
    pub max_rows: U64,
}
impl Limits {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            (1..=120_000).contains(&self.open_deadline_ms.0),
            "CaptureSql open deadline"
        );
        ensure!(
            (1..=3_600_000).contains(&self.total_deadline_ms.0),
            "CaptureSql total deadline"
        );
        ensure!(
            (1..=u32::MAX as u64).contains(&self.vm_steps.0),
            "CaptureSql VM step limit"
        );
        ensure!(
            (1..=MAX_SCHEMA_OBJECTS as u64).contains(&self.schema_objects.0),
            "CaptureSql schema object limit"
        );
        ensure!(
            (1..=PAGE_BYTES as u64).contains(&self.schema_bytes.0),
            "CaptureSql schema byte limit"
        );
        ensure!(
            (1..=PAGE_BYTES as u64).contains(&self.page_bytes.0),
            "CaptureSql page byte limit"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&self.max_cell_bytes.0),
            "CaptureSql cell byte limit"
        );
        ensure!(
            (1..=super::transport::RESULT_BYTES as u64).contains(&self.result_bytes.0),
            "CaptureSql result byte limit"
        );
        ensure!(
            (1..=PAGE_BYTES as u64).contains(&self.inline_bytes.0),
            "CaptureSql inline byte limit"
        );
        ensure!(
            (1..=super::transport::CHUNK_BYTES as u64).contains(&self.chunk_bytes.0),
            "CaptureSql chunk byte limit"
        );
        ensure!(
            (1..=MAX_ROWS as u64).contains(&self.max_rows.0),
            "CaptureSql row limit"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Authority {
    pub protocol: u8,
    pub build: String,
    pub workbench_instance: String,
    pub workbench_generation: String,
    pub filesystem_lease: String,
    pub operation: String,
    pub capture_generation: String,
    pub expires_unix_ms: U64,
    pub capture_root: NativePath,
    pub member: String,
    pub manifest_blake3: String,
    pub revision_id: String,
    pub logical_revision: Revision,
    pub logical_blake3: String,
    pub maximum_bytes: U64,
    pub physical: FileKey,
    pub companion_generation: String,
    pub raw_roster_blake3: String,
    pub limits: Limits,
    pub protected: Vec<FileKey>,
    pub binding_blake3: String,
}
impl Authority {
    fn payload(&self) -> Result<Vec<u8>> {
        crate::lightroom::bounded_json(
            &(
                self.protocol,
                &self.build,
                &self.workbench_instance,
                &self.workbench_generation,
                &self.filesystem_lease,
                &self.operation,
                &self.capture_generation,
                self.expires_unix_ms,
                &self.capture_root,
                &self.member,
                &self.manifest_blake3,
                &self.revision_id,
                &self.logical_revision,
                &self.logical_blake3,
                self.maximum_bytes,
                &self.physical,
                &self.companion_generation,
                &self.raw_roster_blake3,
                self.limits,
                &self.protected,
            ),
            super::transport::AUTHORITY_BYTES,
        )
    }
    pub(crate) fn computed_binding(&self) -> Result<String> {
        Ok(crate::lightroom::digest(&self.payload()?))
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.protocol == 1, "CaptureSql protocol");
        ensure!(
            self.build == crate::lightroom_migration_worker::worker::build_identity(),
            "CaptureSql build identity"
        );
        for (name, value) in [
            ("workbench instance", self.workbench_instance.as_str()),
            ("workbench generation", self.workbench_generation.as_str()),
            ("filesystem lease", self.filesystem_lease.as_str()),
            ("operation", self.operation.as_str()),
            ("capture generation", self.capture_generation.as_str()),
            ("companion generation", self.companion_generation.as_str()),
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 128 && value.is_ascii(),
                "CaptureSql {name}"
            );
        }
        ensure!(self.member == MEMBER, "CaptureSql fixed member");
        crate::catalog_session::validate_path(&self.capture_root)?;
        ensure!(
            self.logical_revision.bytes > 0 && self.logical_revision.bytes <= self.maximum_bytes.0,
            "CaptureSql logical size"
        );
        for digest in [
            &self.manifest_blake3,
            &self.revision_id,
            &self.logical_blake3,
            &self.raw_roster_blake3,
            &self.binding_blake3,
        ] {
            ensure!(super::transport::digest_valid(digest), "CaptureSql digest");
        }
        self.limits.validate()?;
        ensure!(self.protected.len() <= 4096, "CaptureSql protected roster");
        ensure!(
            self.protected.windows(2).all(|w| w[0] < w[1]),
            "CaptureSql protected roster must be sorted and unique"
        );
        ensure!(
            !self.protected.contains(&self.physical),
            "CaptureSql source aliases protected object"
        );
        ensure!(
            self.computed_binding()? == self.binding_blake3,
            "CaptureSql binding"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "query", deny_unknown_fields)]
pub(crate) enum Query {
    SchemaObjects,
    Variables,
    TableRows {
        table_handle: String,
        cursor: Option<Vec<Cell>>,
        limit: U64,
    },
    Current,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObjectKind {
    Table,
    Index,
    View,
    Trigger,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaObject {
    pub kind: ObjectKind,
    #[serde(with = "hex_bytes")]
    pub name: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub table: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub sql: Vec<u8>,
    pub root_page: I64,
    pub without_rowid: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Column {
    pub cid: I64,
    #[serde(with = "hex_bytes")]
    pub name: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub declared_type: Vec<u8>,
    pub not_null: bool,
    #[serde(with = "option_hex_bytes")]
    pub default_sql: Option<Vec<u8>>,
    pub primary_key_ordinal: I64,
    pub hidden: I64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "columns",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum PhysicalCursor {
    PrimaryKey(Vec<U64>),
    RowIdAlias(#[serde(with = "hex_bytes")] Vec<u8>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainedOnlyReason {
    RootPageZero,
    HiddenColumn,
    NoSafeCursor,
    InvalidIdentifier,
    CountFailed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableDescriptor {
    pub table_handle: String,
    pub ordinal: U64,
    pub columns: Vec<Column>,
    pub value_columns: Vec<U64>,
    pub cursor: Option<PhysicalCursor>,
    pub category: String,
    pub expected_count: Option<I64>,
    pub retained_only: Option<RetainedOnlyReason>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaObjects {
    pub authority_binding: String,
    pub schema_roster_blake3: String,
    pub objects: Vec<SchemaObject>,
    pub tables: Vec<TableDescriptor>,
    pub variables: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableRow {
    pub key: Vec<Cell>,
    pub values: Vec<Cell>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableBatch {
    pub authority_binding: String,
    pub schema_roster_blake3: String,
    pub table_handle: String,
    pub request_sequence: U64,
    pub rows: Vec<TableRow>,
    pub next_cursor: Option<Vec<Cell>>,
    pub eof: bool,
    pub observed: U64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TableFailureClass {
    RowBytes,
    CellBytes,
    Statement,
    Step,
    Conversion,
    Count,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableFailure {
    pub authority_binding: String,
    pub schema_roster_blake3: String,
    pub table_handle: String,
    pub cursor: Option<Vec<Cell>>,
    pub class: TableFailureClass,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Current {
    pub authority_binding: String,
    pub schema_roster_blake3: String,
    pub data_version: I64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum TableValue {
    Batch(TableBatch),
    Failure(TableFailure),
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let value = String::deserialize(d)?;
        decode(&value).map_err(serde::de::Error::custom)
    }
    pub(super) fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(out, "{byte:02x}").unwrap();
        }
        out
    }
    pub(super) fn decode(value: &str) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            value.len() % 2 == 0 && value.is_ascii(),
            "invalid hex bytes"
        );
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|v| u8::from_str_radix(std::str::from_utf8(v)?, 16).map_err(Into::into))
            .collect()
    }
}
mod option_hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(|v| super::hex_bytes::hex(v))
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|v| super::hex_bytes::decode(&v).map_err(serde::de::Error::custom))
            .transpose()
    }
}
