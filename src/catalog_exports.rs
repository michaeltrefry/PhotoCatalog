//! Durable photo export authority. Rendering happens outside catalog transactions;
//! only accepted sealed results may publish, under current source/edit/job guards.
use crate::{
    Catalog,
    catalog_edits::{EditRenderIdentity, VariantKey},
    catalog_writer::Priority,
    edit::Recipe,
    image_export::{AlphaPolicy, OutputFormat, OutputProfile, OutputSize, OutputSpec},
    metadata_export::{self, DestinationSnapshot, ExportReceipt, FileRevision, SealedPhotoExport},
    storage_volume::NativePath,
    xmp,
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const SCHEMA: &str = "
CREATE TABLE photo_export_jobs(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,
 state TEXT NOT NULL CHECK(state IN ('building','queued','complete','canceled')),total INTEGER NOT NULL DEFAULT 0,completed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE photo_export_blobs(hash TEXT PRIMARY KEY,raw_length INTEGER NOT NULL CHECK(raw_length>=0 AND raw_length<=16777216),compressed BLOB NOT NULL);
CREATE TABLE photo_export_items(job TEXT NOT NULL REFERENCES photo_export_jobs(id),sequence INTEGER NOT NULL,
 destination TEXT NOT NULL,plan TEXT NOT NULL,authority TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','rendering','sealed','published','failed','canceled')),
 attempt TEXT,seal TEXT,receipt TEXT,error TEXT,
 PRIMARY KEY(job,sequence),UNIQUE(job,destination));
CREATE INDEX photo_export_pending ON photo_export_items(job,sequence) WHERE state IN ('pending','sealed');
CREATE INDEX storage_export_path ON storage_bindings(native_path);
CREATE INDEX storage_export_object ON storage_bindings(file_key) WHERE file_key IS NOT NULL;
";
const BLOB_LIMIT: usize = 16 * 1024 * 1024;
const PLAN_LIMIT: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataSelection {
    Omit,
    /// None creates an empty base only when the asset has no retained models.
    Resolved {
        expected_revision: i64,
        base_model: Option<i64>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportTarget {
    pub key: VariantKey,
    pub expected_revision: i64,
    pub destination: PathBuf,
    pub overwrite: bool,
    pub metadata: MetadataSelection,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StoredProfile {
    Srgb,
    LinearSrgb,
    Icc { blob: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOutput {
    pub size: OutputSize,
    pub format: OutputFormat,
    pub profile: StoredProfile,
    pub alpha: AlphaPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhotoExportPlan {
    pub version: u32,
    pub renderer_identity: String,
    pub identity: EditRenderIdentity,
    pub original: NativePath,
    pub original_revision: FileRevision,
    pub recipe: Recipe,
    pub output: StoredOutput,
    pub metadata: MetadataSelection,
    pub xmp_blob: Option<String>,
    pub destination: DestinationSnapshot,
    pub max_original_bytes: u64,
    pub max_payload_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJob {
    pub sequence: i64,
    pub id: String,
    pub state: String,
    pub total: i64,
    pub completed: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportItem {
    pub sequence: i64,
    pub destination: PathBuf,
    pub state: String,
    pub attempt: Option<String>,
    pub authority: String,
    pub error: Option<String>,
    pub receipt: Option<ExportReceipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportWork {
    pub job: String,
    pub sequence: i64,
    pub attempt: String,
    pub authority: String,
    pub plan: PhotoExportPlan,
}

fn store_blob(db: &Connection, bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len() <= BLOB_LIMIT, "export blob exceeds 16 MiB");
    let hash = blake3::hash(bytes).to_hex().to_string();
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    z.write_all(bytes)?;
    db.execute(
        "INSERT OR IGNORE INTO photo_export_blobs VALUES(?1,?2,?3)",
        params![hash, bytes.len() as i64, z.finish()?],
    )?;
    Ok(hash)
}
fn read_blob(db: &Connection, hash: &str) -> Result<Vec<u8>> {
    let (length, compressed): (i64, Vec<u8>) = db.query_row(
        "SELECT raw_length,compressed FROM photo_export_blobs WHERE hash=?1",
        [hash],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let length = usize::try_from(length).context("negative export blob length")?;
    ensure!(length <= BLOB_LIMIT, "export blob length limit");
    let mut bytes = Vec::new();
    ZlibDecoder::new(compressed.as_slice())
        .take(length as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() == length && blake3::hash(&bytes).to_hex().as_str() == hash,
        "export blob identity mismatch"
    );
    Ok(bytes)
}
fn job(db: &Connection, id: &str) -> Result<ExportJob> {
    Ok(db.query_row(
        "SELECT sequence,state,total,completed FROM photo_export_jobs WHERE id=?1",
        [id],
        |r| {
            Ok(ExportJob {
                sequence: r.get(0)?,
                id: id.into(),
                state: r.get(1)?,
                total: r.get(2)?,
                completed: r.get(3)?,
            })
        },
    )?)
}
fn checked_plan(bytes: &str, authority: &str) -> Result<PhotoExportPlan> {
    ensure!(
        bytes.len() <= PLAN_LIMIT && blake3::hash(bytes.as_bytes()).to_hex().as_str() == authority,
        "export plan identity mismatch"
    );
    let plan: PhotoExportPlan = serde_json::from_str(bytes)?;
    ensure!(
        plan.version == 1 && plan.recipe.validate()?.digest() == plan.identity.recipe_digest,
        "export recipe binding mismatch"
    );
    Ok(plan)
}
fn check_original(db: &Connection, plan: &PhotoExportPlan) -> Result<()> {
    let path = plan.original.to_path()?;
    let current = metadata_export::inspect_file_revision(&path, plan.max_original_bytes)?;
    ensure!(
        current == plan.original_revision,
        "original changed since export planning"
    );
    protect_destination(db, &plan.destination.destination, &path)?;
    Ok(())
}
fn protect_destination(db: &Connection, destination: &Path, original: &Path) -> Result<()> {
    ensure!(
        destination != original.canonicalize()?,
        "export destination is the original"
    );
    let encoded = serde_json::to_string(&NativePath::from_path(destination))?;
    let known: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM storage_bindings WHERE native_path=?1)",
        [encoded],
        |r| r.get(0),
    )?;
    ensure!(!known, "export destination is a catalog original");
    if let Ok(m) = std::fs::symlink_metadata(destination) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "destination must be an ordinary file"
        );
        let key = crate::storage_volume::object_key(destination, &m)?;
        let known: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM storage_bindings WHERE file_key=?1)",
            [format!("{}:{}", key.0, key.1)],
            |r| r.get(0),
        )?;
        ensure!(!known, "export destination aliases a catalog original");
        let source = crate::storage_volume::object_key(original, &std::fs::metadata(original)?)?;
        ensure!(key != source, "export destination aliases the source");
    }
    Ok(())
}
fn metadata_current(db: &Connection, plan: &PhotoExportPlan) -> Result<()> {
    if let MetadataSelection::Resolved {
        expected_revision, ..
    } = plan.metadata
    {
        let revision: i64 = db.query_row(
            "SELECT COALESCE((SELECT revision FROM metadata_assets WHERE asset_id=?1),0)",
            [&plan.identity.key.asset_id],
            |r| r.get(0),
        )?;
        ensure!(
            revision == expected_revision,
            "metadata changed since export planning"
        );
    }
    let original: String = db.query_row(
        "SELECT native_path FROM storage_bindings WHERE asset_id=?1",
        [&plan.identity.key.asset_id],
        |r| r.get(0),
    )?;
    ensure!(
        serde_json::from_str::<NativePath>(&original)? == plan.original,
        "original was relinked since export planning"
    );
    Ok(())
}
fn finish_item(db: &Connection, job_id: &str) -> Result<()> {
    db.execute(
        "UPDATE photo_export_jobs SET completed=completed+1 WHERE id=?1",
        [job_id],
    )?;
    db.execute("UPDATE photo_export_jobs SET state='complete' WHERE id=?1 AND state='queued' AND total=completed",[job_id])?;
    Ok(())
}
impl Catalog {
    pub fn begin_photo_export(&mut self) -> Result<ExportJob> {
        let id = uuid::Uuid::new_v4().to_string();
        let _write = self.writers.enter(Priority::Foreground)?;
        self.db.execute(
            "INSERT INTO photo_export_jobs(id,state) VALUES(?1,'building')",
            [&id],
        )?;
        job(&self.db, &id)
    }
    /// Append one independently frozen item. expected_total is a CAS cursor; callers
    /// can append arbitrarily large batches without retaining a library-sized list.
    pub fn append_photo_export(
        &mut self,
        id: &str,
        expected_total: i64,
        target: &ExportTarget,
        output: &OutputSpec,
        max_original_bytes: u64,
        max_payload_bytes: u64,
    ) -> Result<ExportItem> {
        ensure!(
            max_original_bytes > 0 && max_payload_bytes > 0,
            "export byte limits must be positive"
        );
        let identity = self.edit_render_identity(&target.key)?;
        ensure!(
            identity.revision == target.expected_revision && identity.source.state == "ready",
            "edit changed or original not ready"
        );
        let recipe = self.edit_variant(&target.key)?.recipe;
        ensure!(
            recipe.validate()?.digest() == identity.recipe_digest,
            "edit changed during plan"
        );
        let original = self.preview_original_path(&target.key.asset_id)?;
        let path = original.to_path()?;
        let original_revision = metadata_export::inspect_file_revision(&path, max_original_bytes)?;
        ensure!(
            identity.source.fingerprint.as_deref() == Some(&original_revision.digest),
            "original fingerprint changed"
        );
        let destination =
            metadata_export::snapshot_photo_destination(&target.destination, max_payload_bytes)?;
        ensure!(
            !destination.destination.starts_with(&self.root),
            "export destination is inside the catalog"
        );
        ensure!(
            target.overwrite || destination.expected.is_none(),
            "destination exists; explicit overwrite required"
        );
        let extension = destination
            .destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        ensure!(
            match output.format {
                OutputFormat::Jpeg { .. } => ["jpg", "jpeg"].contains(&extension.as_str()),
                OutputFormat::Png { .. } => extension == "png",
                OutputFormat::Tiff { .. } => ["tif", "tiff"].contains(&extension.as_str()),
            },
            "output extension does not match format"
        );
        let packet = match target.metadata {
            MetadataSelection::Omit => None,
            MetadataSelection::Resolved {
                expected_revision,
                base_model: Some(base),
            } => Some(
                self.resolved_export_xmp(&target.key.asset_id, expected_revision, base)?
                    .0,
            ),
            MetadataSelection::Resolved {
                expected_revision,
                base_model: None,
            } => {
                let view = self.metadata(&target.key.asset_id)?;
                ensure!(
                    view.revision == expected_revision && view.fields.is_empty(),
                    "select a full metadata model"
                );
                let count:i64=self.db.query_row("SELECT COUNT(*) FROM metadata_models m JOIN metadata_observations o ON o.id=m.observation_id JOIN metadata_sources s ON s.id=o.source_id WHERE s.asset_id=?1",[&target.key.asset_id],|r|r.get(0))?;
                ensure!(
                    count == 0,
                    "retained metadata requires explicit full base selection"
                );
                Some(xmp::empty_packet()?)
            }
        };
        let guarded = identity.clone();
        self.with_edit_transaction(&guarded,Priority::Foreground,|tx|{
            let j=job(tx,id)?;ensure!(j.state=="building" && j.total==expected_total,"export job changed or sealed");
            protect_destination(tx,&destination.destination,&path)?;
            let profile=match &output.profile {OutputProfile::Srgb=>StoredProfile::Srgb,OutputProfile::LinearSrgb=>StoredProfile::LinearSrgb,OutputProfile::Icc{bytes}=>StoredProfile::Icc{blob:store_blob(tx,bytes)?}};
            let xmp_blob=packet.as_deref().map(|b|store_blob(tx,b)).transpose()?;
            let plan=PhotoExportPlan{version:1,renderer_identity:crate::photo_render::output_renderer_identity().to_owned(),identity,original,original_revision,recipe,output:StoredOutput{size:output.size,format:output.format,profile,alpha:output.alpha},metadata:target.metadata.clone(),xmp_blob,destination,max_original_bytes,max_payload_bytes};
            metadata_current(tx,&plan)?;
            let encoded=serde_json::to_string(&plan)?;ensure!(encoded.len()<=PLAN_LIMIT,"export plan limit");
            let authority=blake3::hash(encoded.as_bytes()).to_hex().to_string();let sequence=j.total.checked_add(1).context("export job exhausted")?;
            let destination_json=serde_json::to_string(&NativePath::from_path(&plan.destination.destination))?;
            tx.execute("INSERT INTO photo_export_items(job,sequence,destination,plan,authority,state) VALUES(?1,?2,?3,?4,?5,'pending')",params![id,sequence,destination_json,encoded,authority])?;
            tx.execute("UPDATE photo_export_jobs SET total=?1 WHERE id=?2",params![sequence,id])?;
            Ok(ExportItem{sequence,destination:plan.destination.destination,state:"pending".into(),attempt:None,authority,error:None,receipt:None})
        })?.context("edit/source changed during export planning")
    }
    pub fn seal_photo_export_job(&mut self, id: &str, expected_total: i64) -> Result<ExportJob> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let changed=self.db.execute("UPDATE photo_export_jobs SET state='queued' WHERE id=?1 AND total=?2 AND total>0 AND state='building'",params![id,expected_total])?;
        ensure!(changed == 1, "export job changed, empty or already sealed");
        job(&self.db, id)
    }
    pub fn photo_export_job(&self, id: &str) -> Result<ExportJob> {
        job(&self.db, id)
    }
    pub fn photo_export_jobs(&self, after: i64, limit: usize) -> Result<Vec<ExportJob>> {
        ensure!(
            after >= 0 && (1..=200).contains(&limit),
            "export job page bounds"
        );
        self.db
            .prepare(
                "SELECT id FROM photo_export_jobs WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            )?
            .query_map(params![after, limit as i64], |r| r.get::<_, String>(0))?
            .map(|id| job(&self.db, &id?))
            .collect()
    }
    pub fn photo_export_items(
        &self,
        id: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<ExportItem>> {
        ensure!(
            after >= 0 && (1..=200).contains(&limit),
            "export item page bounds"
        );
        job(&self.db, id)?;
        self.db.prepare("SELECT sequence,destination,state,attempt,authority,error,receipt FROM photo_export_items WHERE job=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?.query_map(params![id,after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<String>>(6)?)))?.map(|r|{let(sequence,destination,state,attempt,authority,error,receipt)=r?;Ok(ExportItem{sequence,destination:serde_json::from_str::<NativePath>(&destination)?.to_path()?,state,attempt,authority,error,receipt:receipt.map(|s|serde_json::from_str(&s)).transpose()?})}).collect()
    }
    pub fn photo_export_plan(&self, id: &str, sequence: i64) -> Result<(PhotoExportPlan, String)> {
        let (encoded, authority): (String, String) = self.db.query_row(
            "SELECT plan,authority FROM photo_export_items WHERE job=?1 AND sequence=?2",
            params![id, sequence],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((checked_plan(&encoded, &authority)?, authority))
    }
    pub fn photo_export_inputs(
        &self,
        plan: &PhotoExportPlan,
    ) -> Result<(OutputSpec, Option<Vec<u8>>)> {
        let profile = match &plan.output.profile {
            StoredProfile::Srgb => OutputProfile::Srgb,
            StoredProfile::LinearSrgb => OutputProfile::LinearSrgb,
            StoredProfile::Icc { blob } => OutputProfile::Icc {
                bytes: read_blob(&self.db, blob)?,
            },
        };
        Ok((
            OutputSpec {
                size: plan.output.size,
                format: plan.output.format,
                profile,
                alpha: plan.output.alpha,
            },
            plan.xmp_blob
                .as_deref()
                .map(|h| read_blob(&self.db, h))
                .transpose()?,
        ))
    }
    pub fn claim_photo_export(&mut self, id: &str) -> Result<Option<ExportWork>> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(job(&tx, id)?.state == "queued", "export job is not queued");
        let row:Option<(i64,String,String)>=tx.query_row("SELECT sequence,plan,authority FROM photo_export_items WHERE job=?1 AND state='pending' ORDER BY sequence LIMIT 1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((sequence, encoded, authority)) = row else {
            return Ok(None);
        };
        let plan = checked_plan(&encoded, &authority)?;
        let attempt = uuid::Uuid::new_v4().to_string();
        tx.execute("UPDATE photo_export_items SET state='rendering',attempt=?1 WHERE job=?2 AND sequence=?3",params![attempt,id,sequence])?;
        tx.commit()?;
        Ok(Some(ExportWork {
            job: id.into(),
            sequence,
            attempt,
            authority,
            plan,
        }))
    }
    /// Called only by the owning executor after complete encoding, source verification
    /// and durable staging. An orphan seal alone never authorizes publication.
    pub fn accept_photo_export_seal(
        &mut self,
        work: &ExportWork,
        seal: &SealedPhotoExport,
    ) -> Result<()> {
        checked_plan(&serde_json::to_string(&work.plan)?, &work.authority)?;
        ensure!(
            seal.authority_digest == work.authority
                && seal.snapshot == work.plan.destination
                && seal.max_payload_bytes == work.plan.max_payload_bytes,
            "seal does not match export authority"
        );
        self.with_edit_transaction(&work.plan.identity,Priority::Foreground,|tx|{
            metadata_current(tx,&work.plan)?;ensure!(job(tx,&work.job)?.state=="queued","export canceled");
            check_original(tx,&work.plan)?;
            let changed=tx.execute("UPDATE photo_export_items SET state='sealed',seal=?1 WHERE job=?2 AND sequence=?3 AND state='rendering' AND attempt=?4 AND authority=?5",params![serde_json::to_string(seal)?,work.job,work.sequence,work.attempt,work.authority])?;
            ensure!(changed==1,"export attempt changed or canceled");Ok(())
        })?.context("edit/source changed before accepting export")
    }
    pub fn publish_photo_export_item(&mut self, id: &str, sequence: i64) -> Result<ExportReceipt> {
        let (plan, authority) = self.photo_export_plan(id, sequence)?;
        self.with_edit_transaction(&plan.identity,Priority::Foreground,|tx|{
            metadata_current(tx,&plan)?;ensure!(job(tx,id)?.state=="queued","export canceled");
            check_original(tx,&plan)?;
            let encoded:String=tx.query_row("SELECT seal FROM photo_export_items WHERE job=?1 AND sequence=?2 AND state='sealed' AND authority=?3",params![id,sequence,authority],|r|r.get(0))?;
            let sealed:SealedPhotoExport=serde_json::from_str(&encoded)?;
            ensure!(sealed.authority_digest==authority && sealed.snapshot==plan.destination,"stored seal authority mismatch");
            protect_destination(tx,&plan.destination.destination,&plan.original.to_path()?)?;
            let receipt=metadata_export::publish_photo_export(&sealed)?;
            let state = if receipt.state == metadata_export::ExportState::Published { "published" } else { "failed" };
            let error = (state == "failed").then(|| format!("publication ended in {:?}", receipt.state));
            tx.execute("UPDATE photo_export_items SET state=?1,receipt=?2,error=?3 WHERE job=?4 AND sequence=?5",params![state,serde_json::to_string(&receipt)?,error,id,sequence])?;
            finish_item(tx,id)?;Ok(receipt)
        })?.context("edit/source changed before export publication")
    }
    pub fn fail_photo_export(&mut self, work: &ExportWork, error: &str) -> Result<()> {
        ensure!(error.len() <= 8192, "export error detail limit");
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE photo_export_items SET state='failed',error=?1 WHERE job=?2 AND sequence=?3 AND state='rendering' AND attempt=?4 AND authority=?5",params![error,work.job,work.sequence,work.attempt,work.authority])?;
        ensure!(changed == 1, "export attempt is no longer rendering");
        finish_item(&tx, &work.job)?;
        tx.commit()?;
        Ok(())
    }
    pub fn cancel_photo_export_job(&mut self, id: &str) -> Result<ExportJob> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = job(&tx, id)?;
        ensure!(current.state != "complete", "export job is complete");
        tx.execute(
            "UPDATE photo_export_jobs SET state='canceled' WHERE id=?1",
            [id],
        )?;
        // Sealed entries retain their exact state/evidence for explicit restore.
        tx.execute(
            "UPDATE photo_export_items SET state='canceled' WHERE job=?1 AND state='pending'",
            [id],
        )?;
        let result = job(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "catalog_exports/tests.rs"]
mod tests;
