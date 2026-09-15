//! CaptureSql's single immutable connection and raw guard. Construction is
//! closed before the custom source lock is acquired; reads select only from the
//! admitted immutable roster by generation-bound handle.
use super::capture_wire as wire;
use crate::{
    application::{I64, U64},
    lightroom::{PAGE_BYTES, plan::Cell, source::Source},
    lightroom_migration_worker::identity::FileKey,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::ValueRef};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct AdmittedTable {
    name: String,
    columns: Vec<String>,
    keys: Vec<String>,
}

pub(super) struct CaptureSource {
    db: Connection,
    guard: Source,
    authority: wire::Authority,
    schema: wire::SchemaObjects,
    tables: BTreeMap<String, AdmittedTable>,
    initial_data_version: i64,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}

fn bytes(value: ValueRef<'_>) -> Result<Vec<u8>> {
    Ok(match value {
        ValueRef::Text(v) | ValueRef::Blob(v) => v.to_vec(),
        _ => anyhow::bail!("schema field is not byte text"),
    })
}
fn optional_bytes(value: ValueRef<'_>) -> Result<Option<Vec<u8>>> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Text(v) | ValueRef::Blob(v) => Some(v.to_vec()),
        _ => anyhow::bail!("schema default is not byte text"),
    })
}
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut v = path.as_os_str().to_os_string();
    v.push(suffix);
    v.into()
}
fn no_companions(path: &Path) -> Result<()> {
    for candidate in [
        append(path, "-wal"),
        append(path, "-shm"),
        append(path, "-journal"),
    ] {
        match std::fs::symlink_metadata(&candidate) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => anyhow::bail!("CaptureSql companion appeared"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn category(table: &str) -> String {
    match table {
        "Adobe_images" => "images_and_virtual_copies",
        "AgLibraryFile" => "files",
        "AgLibraryFolder" | "AgLibraryRootFolder" => "folders",
        "AgLibraryKeyword" | "AgLibraryKeywordImage" | "AgLibraryKeywordSynonym" => {
            "keywords_hierarchy_membership_synonyms"
        }
        "AgLibraryCollection" | "AgLibraryCollectionImage" => "collections_membership_order",
        "Adobe_imageDevelopSettings" | "Adobe_imageDevelopBeforeSettings" => {
            "develop_retained_only"
        }
        "Adobe_libraryImageDevelopHistoryStep" => "history_retained_only",
        "Adobe_libraryImageDevelopSnapshot" => "snapshots_retained_only",
        "Adobe_AdditionalMetadata" => "catalog_xmp",
        "AgLibraryCollectionContent" => "smart_collection_instructions_retained_only",
        n if n.starts_with("AgHarvested")
            || n.starts_with("AgInterned")
            || n == "AgLibraryIPTC" =>
        {
            "photographic_metadata"
        }
        _ => "unsupported_retained_only",
    }
    .into()
}
fn cell(value: ValueRef<'_>, maximum: usize) -> Result<Cell> {
    Ok(match value {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(v) => Cell::Integer(v),
        ValueRef::Real(v) => Cell::RealBits(v.to_bits()),
        ValueRef::Text(v) => {
            ensure!(v.len() <= maximum, "text cell exceeds byte limit");
            Cell::Text(v.into())
        }
        ValueRef::Blob(v) => {
            ensure!(v.len() <= maximum, "blob cell exceeds byte limit");
            Cell::Blob(v.into())
        }
    })
}

impl CaptureSource {
    pub(super) fn open(authority: wire::Authority, cancel: Arc<AtomicBool>) -> Result<Self> {
        authority.validate()?;
        ensure!(!cancel.load(Ordering::Acquire), "CaptureSql canceled");
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        ensure!(
            now <= u128::from(authority.expires_unix_ms.0),
            "CaptureSql authority expired"
        );
        let root = authority.capture_root.to_path()?;
        ensure!(root.is_absolute(), "CaptureSql root must be absolute");
        let path = root.join(wire::MEMBER);
        let mut guard = Source::open(&path, authority.maximum_bytes.0)?;
        ensure!(
            authority.logical_revision == guard.before,
            "CaptureSql logical revision differs"
        );
        ensure!(
            FileKey::of(&guard.file)? == authority.physical,
            "CaptureSql physical identity differs"
        );
        let digest = guard.copy_and_hash_controlled(None, || {
            ensure!(!cancel.load(Ordering::Acquire), "CaptureSql canceled")
        })?;
        ensure!(
            digest == authority.logical_blake3,
            "CaptureSql logical digest differs"
        );
        no_companions(&path)?;
        let db = Connection::open_with_flags(
            crate::lightroom::plan::uri(&path)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        crate::catalog_storage::verify_database_object(&db, &guard.file)
            .context("CaptureSql opened object")?;
        crate::lightroom::plan::restrict(
            &db,
            &crate::lightroom::Limits {
                max_cell_bytes: usize::try_from(authority.limits.max_cell_bytes.0)?,
                ..Default::default()
            },
        )?;
        db.pragma_update(None, "query_only", true)?;
        db.pragma_update(None, "temp_store", "MEMORY")?;
        db.busy_timeout(Duration::ZERO)?;
        guard.verify()?;
        no_companions(&path)?;
        guard.lock(0x4000_0000, 512)?;
        guard.verify()?;
        no_companions(&path)?;
        crate::catalog_storage::verify_database_object(&db, &guard.file)?;
        let initial_data_version = db.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let deadline = Instant::now() + Duration::from_millis(authority.limits.total_deadline_ms.0);
        let progress_cancel = cancel.clone();
        let progress_deadline = deadline;
        let progress_steps = Arc::new(AtomicU64::new(0));
        let counter = progress_steps.clone();
        let maximum_steps = authority.limits.vm_steps.0;
        db.progress_handler(
            1000,
            Some(move || {
                progress_cancel.load(Ordering::Acquire)
                    || Instant::now() >= progress_deadline
                    || counter.fetch_add(1000, Ordering::AcqRel) >= maximum_steps
            }),
        )?;
        let (schema, tables) = Self::admit_schema(&db, &authority)?;
        let value = Self {
            db,
            guard,
            authority,
            schema,
            tables,
            initial_data_version,
            cancel,
            deadline,
        };
        value.verify()?;
        Ok(value)
    }

    fn verify(&self) -> Result<()> {
        ensure!(!self.cancel.load(Ordering::Acquire), "CaptureSql canceled");
        ensure!(Instant::now() < self.deadline, "CaptureSql deadline");
        self.guard.verify()?;
        no_companions(&self.guard.path)?;
        crate::catalog_storage::verify_database_object(&self.db, &self.guard.file)?;
        let current: i64 = self.db.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        ensure!(
            current == self.initial_data_version,
            "CaptureSql data version changed"
        );
        Ok(())
    }

    fn admit_schema(
        db: &Connection,
        authority: &wire::Authority,
    ) -> Result<(wire::SchemaObjects, BTreeMap<String, AdmittedTable>)> {
        let max_objects = usize::try_from(authority.limits.schema_objects.0)?;
        let max_bytes = usize::try_from(authority.limits.schema_bytes.0)?;
        let mut without = BTreeMap::new();
        let mut list = db.prepare("PRAGMA table_list")?;
        let mut rows = list.query([])?;
        while let Some(row) = rows.next()? {
            if bytes(row.get_ref(0)?)? != b"main" {
                continue;
            }
            let name = bytes(row.get_ref(1)?)?;
            ensure!(
                without.len() < max_objects,
                "CaptureSql schema object count"
            );
            without.insert(name, row.get::<_, i64>(4)? != 0);
        }
        drop(rows);
        drop(list);
        let mut objects = Vec::new();
        let mut total = 0usize;
        let mut statement = db.prepare("SELECT type,name,tbl_name,coalesce(sql,''),rootpage FROM sqlite_schema ORDER BY type,name LIMIT ?1")?;
        let mut rows = statement.query([i64::try_from(max_objects)?
            .checked_add(1)
            .context("schema count")?])?;
        while let Some(row) = rows.next()? {
            ensure!(
                objects.len() < max_objects,
                "CaptureSql schema object count"
            );
            let kind_bytes = bytes(row.get_ref(0)?)?;
            let kind = match kind_bytes.as_slice() {
                b"table" => wire::ObjectKind::Table,
                b"index" => wire::ObjectKind::Index,
                b"view" => wire::ObjectKind::View,
                b"trigger" => wire::ObjectKind::Trigger,
                _ => continue,
            };
            let name = bytes(row.get_ref(1)?)?;
            let table = bytes(row.get_ref(2)?)?;
            let sql = bytes(row.get_ref(3)?)?;
            total = total
                .checked_add(name.len())
                .and_then(|n| n.checked_add(table.len()))
                .and_then(|n| n.checked_add(sql.len()))
                .context("CaptureSql schema bytes")?;
            ensure!(total <= max_bytes, "CaptureSql schema byte limit");
            objects.push(wire::SchemaObject {
                kind,
                without_rowid: without.get(&name).copied().unwrap_or(false),
                name,
                table,
                sql,
                root_page: I64(row.get(4)?),
            });
        }
        drop(rows);
        drop(statement);
        let stable = crate::lightroom::digest(&crate::lightroom::bounded_json(
            &objects,
            max_bytes
                .checked_mul(6)
                .context("schema encoding bound")?
                .max(1024),
        )?);
        let mut tables = BTreeMap::new();
        let mut descriptors = Vec::new();
        for object in objects
            .iter()
            .filter(|v| matches!(v.kind, wire::ObjectKind::Table))
        {
            let ordinal = descriptors.len();
            let Some(name) = std::str::from_utf8(&object.name).ok().map(str::to_owned) else {
                descriptors.push(wire::TableDescriptor {
                    table_handle: Self::handle(authority, &stable, ordinal, object)?,
                    ordinal: U64(ordinal as u64),
                    columns: vec![],
                    value_columns: vec![],
                    cursor: None,
                    category: "unsupported_retained_only".into(),
                    expected_count: None,
                    retained_only: Some(wire::RetainedOnlyReason::InvalidIdentifier),
                });
                continue;
            };
            let mut columns = Vec::new();
            let mut names = Vec::new();
            let mut info = db.prepare(&format!("PRAGMA table_xinfo({})", quote(&name)))?;
            let mut info_rows = info.query([])?;
            while let Some(row) = info_rows.next()? {
                let raw_name = bytes(row.get_ref(1)?)?;
                let utf8 = std::str::from_utf8(&raw_name).ok().map(str::to_owned);
                names.push(utf8);
                columns.push(wire::Column {
                    cid: I64(row.get(0)?),
                    name: raw_name,
                    declared_type: bytes(row.get_ref(2)?)?,
                    not_null: row.get::<_, i64>(3)? != 0,
                    default_sql: optional_bytes(row.get_ref(4)?)?,
                    primary_key_ordinal: I64(row.get(5)?),
                    hidden: I64(row.get(6)?),
                });
            }
            drop(info_rows);
            drop(info);
            let mut reason = if object.root_page.0 == 0 {
                Some(wire::RetainedOnlyReason::RootPageZero)
            } else if columns.iter().any(|v| v.hidden.0 != 0) {
                Some(wire::RetainedOnlyReason::HiddenColumn)
            } else if names.iter().any(Option::is_none) {
                Some(wire::RetainedOnlyReason::InvalidIdentifier)
            } else {
                None
            };
            let names: Vec<String> = names.into_iter().flatten().collect();
            let cursor = if reason.is_none() && object.without_rowid {
                let mut pk: Vec<_> = columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.primary_key_ordinal.0 > 0)
                    .map(|(i, c)| (c.primary_key_ordinal.0, U64(i as u64)))
                    .collect();
                pk.sort_by_key(|v| v.0);
                if pk.is_empty() {
                    None
                } else {
                    Some(wire::PhysicalCursor::PrimaryKey(
                        pk.into_iter().map(|v| v.1).collect(),
                    ))
                }
            } else if reason.is_none() {
                ["_rowid_", "rowid", "oid"]
                    .into_iter()
                    .find(|candidate| !names.iter().any(|v| v.eq_ignore_ascii_case(candidate)))
                    .map(|v| wire::PhysicalCursor::RowIdAlias(v.as_bytes().to_vec()))
            } else {
                None
            };
            if reason.is_none() && cursor.is_none() {
                reason = Some(wire::RetainedOnlyReason::NoSafeCursor);
            }
            let expected_count = if reason.is_none() {
                match db.query_row(&format!("SELECT count(*) FROM {}", quote(&name)), [], |r| {
                    r.get::<_, i64>(0)
                }) {
                    Ok(v) => Some(I64(v)),
                    Err(_) => {
                        reason = Some(wire::RetainedOnlyReason::CountFailed);
                        None
                    }
                }
            } else {
                None
            };
            let descriptor = wire::TableDescriptor {
                table_handle: Self::handle(authority, &stable, ordinal, object)?,
                ordinal: U64(ordinal as u64),
                value_columns: (0..columns.len()).map(|v| U64(v as u64)).collect(),
                cursor,
                category: category(&name),
                expected_count,
                retained_only: reason,
                columns,
            };
            if descriptor.retained_only.is_none() {
                let keys = match descriptor.cursor.as_ref().unwrap() {
                    wire::PhysicalCursor::PrimaryKey(indexes) => indexes
                        .iter()
                        .map(|i| names[usize::try_from(i.0).unwrap()].clone())
                        .collect(),
                    wire::PhysicalCursor::RowIdAlias(v) => {
                        vec![String::from_utf8(v.clone()).unwrap()]
                    }
                };
                tables.insert(
                    descriptor.table_handle.clone(),
                    AdmittedTable {
                        name,
                        columns: names,
                        keys,
                    },
                );
            }
            descriptors.push(descriptor);
        }
        let variables = Self::variables(db, &objects)?;
        Ok((
            wire::SchemaObjects {
                authority_binding: authority.binding_blake3.clone(),
                schema_roster_blake3: stable,
                objects,
                tables: descriptors,
                variables,
            },
            tables,
        ))
    }
    fn handle(
        authority: &wire::Authority,
        schema: &str,
        ordinal: usize,
        object: &wire::SchemaObject,
    ) -> Result<String> {
        Ok(crate::lightroom::digest(&crate::lightroom::bounded_json(
            &(&authority.binding_blake3, schema, ordinal, object),
            PAGE_BYTES,
        )?))
    }
    fn variables(
        db: &Connection,
        objects: &[wire::SchemaObject],
    ) -> Result<BTreeMap<String, String>> {
        if !objects.iter().any(|v| {
            matches!(v.kind, wire::ObjectKind::Table)
                && v.name == b"Adobe_variablesTable"
                && v.root_page.0 > 0
        }) {
            return Ok(BTreeMap::new());
        }
        let mut out = BTreeMap::new();
        let Ok(mut statement)=db.prepare("SELECT name,value FROM Adobe_variablesTable WHERE name IN ('Adobe_DBVersion','Adobe_storeProviderID')") else { return Ok(out) };
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if let (ValueRef::Text(k), ValueRef::Text(v)) = (row.get_ref(0)?, row.get_ref(1)?) {
                if let (Ok(k), Ok(v)) = (std::str::from_utf8(k), std::str::from_utf8(v)) {
                    out.insert(k.into(), v.into());
                }
            }
        }
        Ok(out)
    }
    pub(super) fn schema(&self) -> wire::SchemaObjects {
        self.schema.clone()
    }
    pub(super) fn binding(&self) -> &str {
        &self.authority.binding_blake3
    }
    pub(super) fn schema_digest(&self) -> &str {
        &self.schema.schema_roster_blake3
    }
    pub(super) fn result_limit(&self) -> Result<usize> {
        Ok(usize::try_from(self.authority.limits.result_bytes.0)?)
    }
    pub(super) fn variables_value(&self) -> BTreeMap<String, String> {
        self.schema.variables.clone()
    }
    pub(super) fn current(&self) -> Result<wire::Current> {
        self.verify()?;
        Ok(wire::Current {
            authority_binding: self.authority.binding_blake3.clone(),
            schema_roster_blake3: self.schema.schema_roster_blake3.clone(),
            data_version: I64(self.initial_data_version),
        })
    }
    pub(super) fn table_rows(
        &self,
        handle: String,
        cursor: Option<Vec<Cell>>,
        limit: usize,
        sequence: u64,
    ) -> Result<std::result::Result<wire::TableBatch, wire::TableFailure>> {
        self.verify()?;
        ensure!(
            (1..=usize::try_from(self.authority.limits.max_rows.0)?).contains(&limit),
            "CaptureSql requested rows"
        );
        let table = self
            .tables
            .get(&handle)
            .context("CaptureSql table handle")?;
        ensure!(
            cursor.as_ref().is_none_or(|v| v.len() == table.keys.len()),
            "CaptureSql cursor arity"
        );
        let values = cursor.clone().unwrap_or_default();
        let select = table
            .keys
            .iter()
            .chain(table.columns.iter())
            .map(|v| quote(v))
            .collect::<Vec<_>>()
            .join(",");
        let order = table
            .keys
            .iter()
            .map(|v| quote(v))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = if values.is_empty() {
            String::new()
        } else {
            format!(
                " WHERE ({order}) > ({})",
                vec!["?"; table.keys.len()].join(",")
            )
        };
        let sql = format!(
            "SELECT {select} FROM {}{predicate} ORDER BY {order} LIMIT {limit}",
            quote(&table.name)
        );
        let result = (|| -> Result<Vec<wire::TableRow>> {
            let mut statement = self.db.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut out = Vec::new();
            let mut total = 0usize;
            let max_cell = usize::try_from(self.authority.limits.max_cell_bytes.0)?;
            let page = usize::try_from(self.authority.limits.page_bytes.0)?;
            while let Some(row) = rows.next()? {
                let mut all = Vec::with_capacity(table.keys.len() + table.columns.len());
                let mut size = 0usize;
                for index in 0..table.keys.len() + table.columns.len() {
                    let v = cell(row.get_ref(index)?, max_cell)?;
                    size = size
                        .checked_add(match &v {
                            Cell::Text(v) | Cell::Blob(v) => v
                                .len()
                                .checked_mul(2)
                                .context("CaptureSql cell bytes")?
                                .checked_add(64)
                                .context("CaptureSql cell bytes")?,
                            _ => 64,
                        })
                        .context("CaptureSql row bytes")?;
                    all.push(v);
                }
                ensure!(size <= page, "CaptureSql row bytes");
                if total.checked_add(size).context("CaptureSql page bytes")? > page {
                    break;
                }
                total += size;
                out.push(wire::TableRow {
                    key: all[..table.keys.len()].to_vec(),
                    values: all[table.keys.len()..].to_vec(),
                });
            }
            Ok(out)
        })();
        let rows = match result {
            Ok(v) => v,
            Err(error) => {
                self.verify()?;
                return Ok(Err(wire::TableFailure {
                    authority_binding: self.authority.binding_blake3.clone(),
                    schema_roster_blake3: self.schema.schema_roster_blake3.clone(),
                    table_handle: handle,
                    cursor,
                    class: wire::TableFailureClass::Statement,
                    detail: format!("{error:#}").chars().take(4096).collect(),
                }));
            }
        };
        self.verify()?;
        let next_cursor = rows.last().map(|v| v.key.clone()).or(cursor);
        Ok(Ok(wire::TableBatch {
            authority_binding: self.authority.binding_blake3.clone(),
            schema_roster_blake3: self.schema.schema_roster_blake3.clone(),
            table_handle: handle,
            request_sequence: U64(sequence),
            observed: U64(rows.len() as u64),
            eof: rows.is_empty(),
            rows,
            next_cursor,
        }))
    }
}
