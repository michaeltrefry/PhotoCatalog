//! Durable attempt authority for desktop metadata mutations.
use crate::{Catalog, catalog_images::ImageMetadataIdentity};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

pub(crate) const SCHEMA: &str = "
CREATE TABLE metadata_write_receipts (
  attempt TEXT PRIMARY KEY NOT NULL CHECK(length(attempt)=36),
  request_digest TEXT NOT NULL CHECK(length(request_digest)=64 AND request_digest NOT GLOB '*[^0-9a-f]*'),
  kind TEXT NOT NULL CHECK(kind IN ('edit','resolve','sidecar_plan','sidecar_apply','sidecar_recover','sidecar_restore')),
  owner_json TEXT NOT NULL CHECK(length(CAST(owner_json AS BLOB))<=32768),
  result_version INTEGER NOT NULL CHECK(result_version=1),
  result_json TEXT NOT NULL CHECK(length(CAST(result_json AS BLOB))<=65536),
  created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Owner {
    Image { identity: ImageMetadataIdentity },
    LegacyAsset { asset_id: String, revision: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub attempt: String,
    pub request_digest: String,
    pub kind: String,
    pub owner: Owner,
    pub result: serde_json::Value,
    pub created_at: String,
}

pub(crate) fn validate_attempt(attempt: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(attempt)?;
    ensure!(
        id.hyphenated().to_string() == attempt,
        "attempt must be a canonical UUID"
    );
    Ok(())
}

pub(crate) fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "request digest must be lowercase BLAKE3"
    );
    Ok(())
}

fn validate_operation(operation: &str) -> Result<()> {
    let id = uuid::Uuid::parse_str(operation)?;
    ensure!(
        id.hyphenated().to_string() == operation,
        "metadata plan operation must be a canonical UUID"
    );
    Ok(())
}

pub(crate) struct ExportPlanRow {
    pub asset_id: String,
    pub revision: i64,
    pub base_model: i64,
    pub plan_json: String,
    pub payload_hash: String,
    pub receipt_json: Option<String>,
}

type RawExportPlan = (
    Option<String>,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

pub(crate) fn export_plan_row(db: &Connection, operation: &str) -> Result<ExportPlanRow> {
    validate_operation(operation)?;
    let row: RawExportPlan = db
        .query_row(
            "SELECT CASE WHEN length(CAST(asset_id AS BLOB))<=256 THEN asset_id END,revision,base_model,CASE WHEN length(CAST(plan AS BLOB))<=65536 THEN plan END,CASE WHEN length(CAST(payload_hash AS BLOB))=64 THEN payload_hash END,CASE WHEN length(CAST(receipt AS BLOB))<=65536 THEN receipt END,receipt IS NULL FROM metadata_export_plans WHERE operation=?1",
            [operation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )?;
    let receipt_json = match (row.6, row.5) {
        (true, _) => None,
        (false, Some(value)) => Some(value),
        (false, None) => anyhow::bail!("metadata plan receipt byte limit"),
    };
    Ok(ExportPlanRow {
        asset_id: row.0.context("metadata plan owner byte limit")?,
        revision: row.1,
        base_model: row.2,
        plan_json: row.3.context("metadata plan byte limit")?,
        payload_hash: row.4.context("metadata plan payload hash byte limit")?,
        receipt_json,
    })
}

pub(crate) fn insert(
    db: &Connection,
    attempt: &str,
    request_digest: &str,
    kind: &str,
    owner: &Owner,
    result: &serde_json::Value,
) -> Result<()> {
    validate_attempt(attempt)?;
    validate_digest(request_digest)?;
    ensure!(
        matches!(
            kind,
            "edit"
                | "resolve"
                | "sidecar_plan"
                | "sidecar_apply"
                | "sidecar_recover"
                | "sidecar_restore"
        ),
        "metadata receipt kind"
    );
    let owner = serde_json::to_string(owner)?;
    let result = serde_json::to_string(result)?;
    ensure!(
        owner.len() <= 32 * 1024 && result.len() <= 64 * 1024,
        "metadata receipt byte limit"
    );
    db.execute(
        "INSERT INTO metadata_write_receipts(attempt,request_digest,kind,owner_json,result_version,result_json) VALUES(?1,?2,?3,?4,1,?5)",
        params![attempt, request_digest, kind, owner, result],
    )?;
    Ok(())
}

type RawReceipt = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
);

pub(crate) fn existing(
    db: &Connection,
    attempt: &str,
    request_digest: &str,
) -> Result<Option<Receipt>> {
    validate_attempt(attempt)?;
    validate_digest(request_digest)?;
    let row: Option<RawReceipt> = db
        .query_row(
            "SELECT CASE WHEN length(CAST(attempt AS BLOB))=36 THEN attempt END,CASE WHEN length(CAST(request_digest AS BLOB))=64 THEN request_digest END,CASE WHEN length(CAST(kind AS BLOB))<=32 THEN kind END,CASE WHEN length(CAST(owner_json AS BLOB))<=32768 THEN owner_json END,result_version,CASE WHEN length(CAST(result_json AS BLOB))<=65536 THEN result_json END,CASE WHEN length(CAST(created_at AS BLOB))<=1024 THEN created_at END FROM metadata_write_receipts WHERE attempt=?1",
            [attempt],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .optional()?;
    let Some((attempt, stored_digest, kind, owner, version, result, created_at)) = row else {
        return Ok(None);
    };
    let attempt = attempt.context("metadata receipt attempt byte limit")?;
    let stored_digest = stored_digest.context("metadata receipt digest byte limit")?;
    let kind = kind.context("metadata receipt kind byte limit")?;
    let owner = owner.context("metadata receipt owner byte limit")?;
    let result = result.context("metadata receipt result byte limit")?;
    let created_at = created_at.context("metadata receipt timestamp byte limit")?;
    ensure!(
        stored_digest == request_digest,
        "attempt already belongs to a different request"
    );
    ensure!(
        version == 1 && owner.len() <= 32 * 1024 && result.len() <= 64 * 1024,
        "metadata receipt format"
    );
    Ok(Some(Receipt {
        attempt,
        request_digest: stored_digest,
        kind,
        owner: serde_json::from_str(&owner)?,
        result: serde_json::from_str(&result)?,
        created_at,
    }))
}

pub(crate) type SavedExportPlan = (
    crate::catalog_metadata_write::Owner,
    i64,
    i64,
    crate::metadata_export::ExportPlan,
    Option<crate::metadata_export::ExportReceipt>,
    String,
);

pub(crate) type ExportPlanPage = (
    Vec<(
        i64,
        crate::catalog_metadata_write::Owner,
        i64,
        i64,
        crate::metadata_export::ExportPlan,
        Option<crate::metadata_export::ExportReceipt>,
        String,
    )>,
    Option<i64>,
    usize,
);

impl Catalog {
    pub fn metadata_write_receipt(&self, attempt: &str) -> Result<Option<Receipt>> {
        validate_attempt(attempt)?;
        let digest: Option<Option<String>> = self
            .db
            .query_row(
                "SELECT CASE WHEN length(CAST(request_digest AS BLOB))=64 THEN request_digest END FROM metadata_write_receipts WHERE attempt=?1",
                [attempt],
                |row| row.get(0),
            )
            .optional()?;
        match digest {
            None => Ok(None),
            Some(None) => anyhow::bail!("metadata receipt digest byte limit"),
            Some(Some(digest)) => existing(&self.db, attempt, &digest)
                .and_then(|value| value.context("metadata receipt disappeared").map(Some)),
        }
    }

    pub(crate) fn metadata_export_plan(&self, operation: &str) -> Result<Option<SavedExportPlan>> {
        let row = match export_plan_row(&self.db, operation) {
            Ok(row) => row,
            Err(error)
                if matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::QueryReturnedNoRows)
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let authority = blake3::hash(row.plan_json.as_bytes()).to_hex().to_string();
        let plan = serde_json::from_str(&row.plan_json)?;
        let owner =
            crate::catalog_image_exports::owner(&self.db, operation, &row.asset_id, row.revision)?;
        let receipt = row
            .receipt_json
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        Ok(Some((
            owner,
            row.revision,
            row.base_model,
            plan,
            receipt,
            authority,
        )))
    }

    pub(crate) fn metadata_export_plans(
        &self,
        owner: Option<&crate::catalog_edits::VariantKey>,
        after: i64,
        limit: usize,
        scan_limit: usize,
    ) -> Result<ExportPlanPage> {
        ensure!(
            after >= 0 && (1..=100).contains(&limit) && (limit..=1000).contains(&scan_limit),
            "metadata plan page bounds"
        );
        let mut statement = self.db.prepare("SELECT rowid,CASE WHEN length(CAST(operation AS BLOB))=36 THEN operation END,CASE WHEN length(CAST(asset_id AS BLOB))<=256 THEN asset_id END,revision,base_model,CASE WHEN length(CAST(plan AS BLOB))<=65536 THEN plan END,CASE WHEN length(CAST(receipt AS BLOB))<=65536 THEN receipt END,receipt IS NULL FROM metadata_export_plans WHERE rowid>?1 ORDER BY rowid LIMIT ?2")?;
        let mut rows = statement.query(params![after, i64::try_from(scan_limit + 1)?])?;
        let mut output = Vec::new();
        let mut cursor = after;
        let mut scanned = 0usize;
        while scanned < scan_limit && output.len() < limit {
            let Some(row) = rows.next()? else { break };
            scanned += 1;
            cursor = row.get(0)?;
            let operation: String = row
                .get::<_, Option<String>>(1)?
                .context("metadata plan operation byte limit")?;
            validate_operation(&operation)?;
            let asset: String = row
                .get::<_, Option<String>>(2)?
                .context("metadata plan owner byte limit")?;
            let revision: i64 = row.get(3)?;
            let base_model: i64 = row.get(4)?;
            let plan_json: String = row
                .get::<_, Option<String>>(5)?
                .context("metadata plan byte limit")?;
            let receipt_json: Option<String> = row.get(6)?;
            let receipt_is_null: bool = row.get(7)?;
            let receipt_json = match (receipt_is_null, receipt_json) {
                (true, _) => None,
                (false, Some(value)) => Some(value),
                (false, None) => anyhow::bail!("metadata plan receipt byte limit"),
            };
            let authority =
                crate::catalog_image_exports::owner(&self.db, &operation, &asset, revision)?;
            if owner.is_some_and(|selected| !matches!(&authority, Owner::Image { identity } if &identity.key == selected)) {
                continue;
            }
            output.push((
                cursor,
                authority,
                revision,
                base_model,
                serde_json::from_str(&plan_json)?,
                receipt_json
                    .map(|value| serde_json::from_str(&value))
                    .transpose()?,
                blake3::hash(plan_json.as_bytes()).to_hex().to_string(),
            ));
        }
        let more = rows.next()?.is_some();
        Ok((output, more.then_some(cursor), scanned))
    }

    pub(crate) fn export_metadata_evidence(
        &mut self,
        identity: &ImageMetadataIdentity,
        observation: i64,
        destination: &crate::storage_volume::NativePath,
        byte_limit: u64,
        packet_limit: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::catalog_session::metadata_files::EvidenceReceipt> {
        crate::catalog_images::require_image_metadata_identity(&self.db, identity)?;
        ensure!(
            (1..=256 * 1024 * 1024).contains(&byte_limit) && (1..=1024).contains(&packet_limit),
            "metadata evidence limits"
        );
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        crate::catalog_images::require_image_metadata_identity(&tx, identity)?;
        let owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata_image_observations WHERE image_id=?1 AND observation_id=?2)",
            params![identity.image_id, observation],
            |row| row.get(0),
        )?;
        ensure!(owned, "observation is not owned by selected image");
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM metadata_packets WHERE observation_id=?1",
            [observation],
            |row| row.get(0),
        )?;
        ensure!(
            count >= 0 && usize::try_from(count)? <= packet_limit,
            "metadata evidence packet limit"
        );
        let mut seal = EvidenceSeal::new(byte_limit);
        emit_evidence_document(&tx, observation, cancel, &mut seal)?;
        let total = seal.bytes;
        let digest = seal.hasher.finalize().to_hex().to_string();
        let managed = self.session.write_metadata_evidence_stream(
            destination,
            total,
            &digest,
            cancel,
            |writer| emit_evidence_document(&tx, observation, cancel, writer),
        )?;
        let receipt = match managed {
            Some(receipt) => receipt,
            None => {
                crate::metadata_export::write_evidence_new_stream(
                    &destination.to_path()?,
                    |writer| emit_evidence_document(&tx, observation, cancel, writer),
                )?;
                crate::catalog_session::metadata_files::EvidenceReceipt {
                    destination: destination.clone(),
                    bytes: crate::application::U64(total),
                    blake3: digest,
                }
            }
        };
        tx.commit()?;
        Ok(receipt)
    }
}

struct EvidenceSeal {
    hasher: blake3::Hasher,
    bytes: u64,
    limit: u64,
}
impl EvidenceSeal {
    fn new(limit: u64) -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            bytes: 0,
            limit,
        }
    }
}
impl Write for EvidenceSeal {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("metadata evidence byte count overflow"))?;
        if next > self.limit {
            return Err(io::Error::other("metadata evidence byte limit"));
        }
        self.hasher.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn emit_evidence_document(
    db: &Connection,
    observation: i64,
    cancel: &std::sync::atomic::AtomicBool,
    writer: &mut dyn Write,
) -> Result<()> {
    writer.write_all(b"[")?;
    let mut statement = db.prepare(
        "SELECT ordinal,CASE WHEN length(CAST(blob_hash AS BLOB))=64 THEN blob_hash END,CASE WHEN length(CAST(descriptor AS BLOB))<=16384 THEN descriptor END FROM metadata_packets WHERE observation_id=?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([observation])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::Acquire),
            "metadata evidence export canceled"
        );
        let ordinal: i64 = row.get(0)?;
        ensure!(ordinal >= 0, "metadata packet ordinal");
        let digest: String = row
            .get::<_, Option<String>>(1)?
            .context("metadata packet digest byte limit")?;
        validate_digest(&digest)?;
        let descriptor: String = row
            .get::<_, Option<String>>(2)?
            .context("metadata packet descriptor byte limit")?;
        let _: serde_json::Value = serde_json::from_str(&descriptor)?;
        let bytes = crate::catalog_metadata::read_blob(db, &digest)?;
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write!(
            writer,
            "{{\"ordinal\":{ordinal},\"descriptor\":{descriptor},\"bytes\":"
        )?;
        serde_json::to_writer(&mut *writer, &bytes)?;
        write!(writer, ",\"blake3\":\"{digest}\"}}")?;
    }
    writer.write_all(b"]")?;
    Ok(())
}
