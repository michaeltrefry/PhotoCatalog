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
 state TEXT NOT NULL CHECK(state IN ('pending','rendering','sealed','published','failed','canceled','restored')),
 attempt TEXT,seal TEXT,receipt TEXT,error TEXT,publication TEXT,
 PRIMARY KEY(job,sequence),UNIQUE(job,destination));
CREATE INDEX photo_export_intents ON photo_export_items(job,sequence) WHERE state='sealed' AND publication IS NOT NULL;
CREATE INDEX photo_export_rendering ON photo_export_items(job,sequence) WHERE state='rendering';
CREATE INDEX photo_export_pending ON photo_export_items(job,state,sequence);
CREATE INDEX storage_export_path ON storage_bindings(native_path);
CREATE INDEX storage_export_object ON storage_bindings(file_key) WHERE file_key IS NOT NULL;
";
const BLOB_LIMIT: usize = 16 * 1024 * 1024;
const PLAN_LIMIT: usize = 128 * 1024;
const NEXT_SEALED: &str = "SELECT sequence FROM photo_export_items WHERE job=?1 AND state='sealed' ORDER BY sequence LIMIT 1";
const NEXT_PENDING: &str = "SELECT sequence,plan,authority FROM photo_export_items WHERE job=?1 AND state='pending' ORDER BY sequence LIMIT 1";

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
    pub alias_limits: crate::catalog_export_alias::AliasLimits,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoExportBoundary {
    Hashing,
    OriginalVerified,
    IntentCommitted,
    Captured,
    CaptureVerified,
    Linked,
    InstalledVerified,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportPublicationMetrics {
    pub original_hash_ms: f64,
    /// Includes admission/SQLite wait, short validation, action and commit.
    pub authority_intervals_ms: Vec<f64>,
    pub publication: metadata_export::PhotoPublicationTimings,
    pub total_ms: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PublicationIntent {
    version: u32,
    token: String,
    authority: String,
}
fn parse_intent(encoded: &str, authority: &str) -> Result<PublicationIntent> {
    let intent: PublicationIntent = serde_json::from_str(encoded)?;
    ensure!(
        intent.version == 1
            && intent.authority == authority
            && uuid::Uuid::parse_str(&intent.token).is_ok(),
        "publication intent differs from authority"
    );
    Ok(intent)
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
fn verify_original(
    plan: &PhotoExportPlan,
    checkpoint: &mut dyn FnMut(u64) -> std::io::Result<()>,
) -> Result<metadata_export::VerifiedFile> {
    let verified = metadata_export::VerifiedFile::read_with_checkpoint(
        &plan.original.to_path()?,
        plan.max_original_bytes,
        checkpoint,
    )?;
    ensure!(
        verified.revision() == &plan.original_revision,
        "original changed since export planning"
    );
    Ok(verified)
}
fn protect_destination(
    db: &Connection,
    destination: &Path,
    original: &Path,
    limits: crate::catalog_export_alias::AliasLimits,
) -> Result<()> {
    crate::catalog_export_alias::protect_destination(db, destination, limits)?;
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
fn publication_current(
    db: &Connection,
    id: &str,
    sequence: i64,
    authority: &str,
    intent: &str,
    plan: &PhotoExportPlan,
) -> Result<()> {
    metadata_current(db, plan)?;
    ensure!(job(db, id)?.state == "queued", "export canceled");
    let current:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3 AND publication=?4 AND state='sealed')",params![id,sequence,authority,intent],|r|r.get(0))?;
    ensure!(current, "publication intent changed");
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
    pub fn reconcile_export_paths(
        &mut self,
        limit: usize,
    ) -> Result<crate::catalog_export_alias::AliasProgress> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = crate::catalog_export_alias::reconcile_paths(&tx, limit)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn rendering_photo_export_attempts(&self, limit: usize) -> Result<Vec<ExportWork>> {
        ensure!(limit > 0 && limit <= 200, "export recovery page limit");
        let mut statement=self.db.prepare("SELECT job,sequence,attempt,plan,authority FROM photo_export_items WHERE state='rendering' ORDER BY job,sequence LIMIT ?1")?;
        let rows = statement.query_map([limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (job, sequence, attempt, encoded, authority) = row?;
            let plan = checked_plan(&encoded, &authority)?;
            result.push(ExportWork {
                job,
                sequence,
                attempt,
                authority,
                plan,
            });
        }
        Ok(result)
    }
    /// One accepted result ready for guarded publication, including after restart.
    pub fn next_sealed_photo_export(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .db
            .query_row(NEXT_SEALED, [id], |r| r.get(0))
            .optional()?)
    }
    /// Recover a single stored rendering authority. An executor must hold the
    /// catalog export lease and retire/reap its transport before fencing it.
    pub fn photo_export_attempt(&self, id: &str, sequence: i64) -> Result<ExportWork> {
        self.photo_export_attempt_if_rendering(id, sequence)?
            .context("export item is not rendering")
    }
    pub fn photo_export_attempt_if_rendering(
        &self,
        id: &str,
        sequence: i64,
    ) -> Result<Option<ExportWork>> {
        let attempt:Option<String>=self.db.query_row("SELECT attempt FROM photo_export_items WHERE job=?1 AND sequence=?2 AND state='rendering'",params![id,sequence],|r|r.get(0)).optional()?;
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        let (plan, authority) = self.photo_export_plan(id, sequence)?;
        Ok(Some(ExportWork {
            job: id.to_owned(),
            sequence,
            attempt,
            authority,
            plan,
        }))
    }
    /// Preserve failed publication evidence without repeatedly retrying it on each
    /// actor tick. Only an explicit recovery request can try its accepted seal again.
    pub fn fail_sealed_photo_export(&mut self, id: &str, sequence: i64, error: &str) -> Result<()> {
        ensure!(error.len() <= 8192, "export error detail limit");
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(tx.execute("UPDATE photo_export_items SET state='failed',error=?1 WHERE job=?2 AND sequence=?3 AND state='sealed'",params![error,id,sequence])?==1,"export is no longer sealed");
        finish_item(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
    /// Explicitly retry publication of an already accepted seal. A canceled job
    /// remains canceled. This never promotes a worker-discovered orphan.
    pub fn retry_sealed_photo_export(&mut self, id: &str, sequence: i64) -> Result<()> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            ["queued", "complete"].contains(&job(&tx, id)?.state.as_str()),
            "export canceled or not sealed"
        );
        ensure!(tx.execute("UPDATE photo_export_items SET state='sealed',error=NULL WHERE job=?1 AND sequence=?2 AND state='failed' AND seal IS NOT NULL",params![id,sequence])?==1,"no accepted failed seal to recover");
        tx.execute(
            "UPDATE photo_export_jobs SET state='queued',completed=completed-1 WHERE id=?1",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Restore captured destination bytes without restoring stale catalog edits.
    /// Bulk verification/durability happen outside the writer; namespace restoration
    /// is guarded by the current alias index and exact accepted operation identity.
    pub fn restore_photo_export_item(&mut self, id: &str, sequence: i64) -> Result<ExportReceipt> {
        let (plan, authority) = self.photo_export_plan(id, sequence)?;
        let (state,encoded,intent):(String,Option<String>,Option<String>)=self.db.query_row(
            "SELECT state,seal,publication FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3",params![id,sequence,authority],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            state != "rendering",
            "stop and fence the worker before restoration"
        );
        let seal = if let Some(encoded) = encoded {
            serde_json::from_str::<SealedPhotoExport>(&encoded)?
        } else {
            metadata_export::read_photo_seal(&plan.destination, &authority)?
        };
        ensure!(
            seal.snapshot == plan.destination && seal.authority_digest == authority,
            "restoration seal authority mismatch"
        );
        let mut session = metadata_export::PhotoPublication::prepare_restore(&seal)?;
        if session.installed() {
            let encoded = intent.context("installed output lacks committed publication intent")?;
            parse_intent(&encoded, &authority)?;
            let receipt = session.verify_installed()?;
            self.finalize_photo_publication(
                id, sequence, &authority, &encoded, &session, &receipt,
            )?;
            return Ok(receipt);
        }
        let namespace = (|| -> Result<()> {
            let _write = self.writers.enter(Priority::Foreground)?;
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let current:String=tx.query_row("SELECT state FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3",params![id,sequence,authority],|r|r.get(0))?;
            ensure!(
                current != "rendering",
                "worker became active before restoration"
            );
            crate::catalog_export_alias::protect_destination(
                &tx,
                &plan.destination.destination,
                plan.alias_limits,
            )?;
            session.restore_link()?;
            tx.commit()?;
            Ok(())
        })();
        let receipt = match namespace {
            Ok(()) => match session.verify_restored() {
                Ok(receipt) => receipt,
                Err(error) => {
                    session.failure_receipt(format!("restoration verification: {error:#}"))
                }
            },
            Err(error) => session.failure_receipt(format!("restoration retained: {error:#}")),
        };
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: String = tx.query_row(
            "SELECT state FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3",
            params![id, sequence, authority],
            |r| r.get(0),
        )?;
        ensure!(
            current != "rendering",
            "worker became active during restoration"
        );
        if receipt.state == metadata_export::ExportState::Restored {
            session.recheck_restored()?;
        }
        let next = if receipt.state == metadata_export::ExportState::Restored {
            "restored"
        } else {
            "failed"
        };
        tx.execute("UPDATE photo_export_items SET state=?1,receipt=?2,error=?3 WHERE job=?4 AND sequence=?5",params![next,serde_json::to_string(&receipt)?,(next=="failed").then(||receipt.detail.clone()),id,sequence])?;
        if ["pending", "sealed"].contains(&current.as_str()) {
            finish_item(&tx, id)?;
        }
        tx.commit()?;
        Ok(receipt)
    }
    /// Bounded recovery queue for intents that may have linked before a crash.
    pub fn photo_export_publication_intents(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        ensure!(
            (1..=200).contains(&limit),
            "publication recovery page limit"
        );
        Ok(self.db.prepare("SELECT job,sequence FROM photo_export_items WHERE state='sealed' AND publication IS NOT NULL ORDER BY job,sequence LIMIT ?1")?
            .query_map([limit as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }
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
        self.append_photo_export_with_alias_limits(
            id,
            expected_total,
            target,
            output,
            max_original_bytes,
            max_payload_bytes,
            Default::default(),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn append_photo_export_with_alias_limits(
        &mut self,
        id: &str,
        expected_total: i64,
        target: &ExportTarget,
        output: &OutputSpec,
        max_original_bytes: u64,
        max_payload_bytes: u64,
        alias_limits: crate::catalog_export_alias::AliasLimits,
    ) -> Result<ExportItem> {
        // One bounded catch-up handles newly imported assets; large catalogs use
        // explicit pages through reconcile_export_paths before planning.
        self.reconcile_export_paths(512)?;
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
            protect_destination(tx,&destination.destination,&path,alias_limits)?;
            let profile=match &output.profile {OutputProfile::Srgb=>StoredProfile::Srgb,OutputProfile::LinearSrgb=>StoredProfile::LinearSrgb,OutputProfile::Icc{bytes}=>StoredProfile::Icc{blob:store_blob(tx,bytes)?}};
            let xmp_blob=packet.as_deref().map(|b|store_blob(tx,b)).transpose()?;
            let plan=PhotoExportPlan{version:1,renderer_identity:crate::photo_render::output_renderer_identity().to_owned(),identity,original,original_revision,recipe,output:StoredOutput{size:output.size,format:output.format,profile,alpha:output.alpha},metadata:target.metadata.clone(),xmp_blob,destination,max_original_bytes,max_payload_bytes,alias_limits};
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
    /// Read-only cancellation hint for an owning actor. Publication separately
    /// revalidates all authority inside its transaction; this is not a permit.
    pub fn photo_export_work_current(&self, work: &ExportWork) -> Result<bool> {
        checked_plan(&serde_json::to_string(&work.plan)?, &work.authority)?;
        let current = self.edit_render_identity(&work.plan.identity.key)?;
        let expected = &work.plan.identity;
        if current.revision != expected.revision
            || current.recipe_digest != expected.recipe_digest
            || current.source.generation != expected.source.generation
            || current.source.fingerprint != expected.source.fingerprint
            || current.source.state != expected.source.state
        {
            return Ok(false);
        }
        if let MetadataSelection::Resolved {
            expected_revision, ..
        } = work.plan.metadata
            && current.source.metadata_revision != expected_revision
        {
            return Ok(false);
        }
        let original: String = self.db.query_row(
            "SELECT native_path FROM storage_bindings WHERE asset_id=?1",
            [&work.plan.identity.key.asset_id],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<NativePath>(&original)? != work.plan.original {
            return Ok(false);
        }
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM photo_export_items i JOIN photo_export_jobs j ON j.id=i.job WHERE i.job=?1 AND i.sequence=?2 AND i.attempt=?3 AND i.authority=?4 AND i.state='rendering' AND j.state='queued')",params![work.job,work.sequence,work.attempt,work.authority],|r|r.get(0))?)
    }
    /// Fence a stopped attempt. The executor must reap its worker before launching
    /// another; a late result with the old token can never be accepted afterward.
    pub fn requeue_photo_export_attempt(&mut self, work: &ExportWork) -> Result<()> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state = if job(&tx, &work.job)?.state == "queued" {
            "pending"
        } else {
            "canceled"
        };
        let changed=tx.execute("UPDATE photo_export_items SET state=?1,attempt=NULL WHERE job=?2 AND sequence=?3 AND state='rendering' AND attempt=?4 AND authority=?5",params![state,work.job,work.sequence,work.attempt,work.authority])?;
        ensure!(changed == 1, "export attempt is no longer owned/rendering");
        tx.commit()?;
        Ok(())
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
        let row: Option<(i64, String, String)> = tx
            .query_row(NEXT_PENDING, [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
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
        self.accept_photo_export_seal_with_hook(work, seal, |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn accept_photo_export_seal_with_hook(
        &mut self,
        work: &ExportWork,
        seal: &SealedPhotoExport,
        mut hook: impl FnMut(PhotoExportBoundary) -> Result<()>,
    ) -> Result<()> {
        checked_plan(&serde_json::to_string(&work.plan)?, &work.authority)?;
        ensure!(
            seal.authority_digest == work.authority
                && seal.snapshot == work.plan.destination
                && seal.max_payload_bytes == work.plan.max_payload_bytes,
            "seal does not match export authority"
        );
        let original = verify_original(&work.plan, &mut |_| {
            hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
        })?;
        // Verify the seal under its operation lease too; acceptance never trusts
        // a serialized caller's claim about payload bytes.
        let _publication =
            metadata_export::PhotoPublication::prepare_with_checkpoint(seal, &mut |_| {
                hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
            })?;
        hook(PhotoExportBoundary::OriginalVerified)?;
        self.with_edit_transaction(&work.plan.identity,Priority::Foreground,|tx|{
            metadata_current(tx,&work.plan)?;ensure!(job(tx,&work.job)?.state=="queued","export canceled");
            original.recheck().context("original changed while waiting for export authority")?;_publication.recheck_payload()?;
            protect_destination(tx,&work.plan.destination.destination,&work.plan.original.to_path()?,work.plan.alias_limits)?;
            ensure!(tx.execute("UPDATE photo_export_items SET state='sealed',seal=?1 WHERE job=?2 AND sequence=?3 AND state='rendering' AND attempt=?4 AND authority=?5",params![serde_json::to_string(seal)?,work.job,work.sequence,work.attempt,work.authority])?==1,"export attempt changed or canceled");Ok(())
        })?.context("edit/source changed before accepting export")
    }
    pub fn publish_photo_export_item(&mut self, id: &str, sequence: i64) -> Result<ExportReceipt> {
        Ok(self.publish_photo_export_item_with_metrics(id, sequence)?.0)
    }
    pub fn publish_photo_export_item_with_metrics(
        &mut self,
        id: &str,
        sequence: i64,
    ) -> Result<(ExportReceipt, ExportPublicationMetrics)> {
        self.publish_photo_export_item_with_hook(id, sequence, |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn publish_photo_export_item_with_hook(
        &mut self,
        id: &str,
        sequence: i64,
        mut hook: impl FnMut(PhotoExportBoundary) -> Result<()>,
    ) -> Result<(ExportReceipt, ExportPublicationMetrics)> {
        let started = std::time::Instant::now();
        let mut metrics = ExportPublicationMetrics::default();
        let (plan, authority) = self.photo_export_plan(id, sequence)?;
        let (state,encoded,previous):(String,String,Option<String>)=self.db.query_row(
            "SELECT state,seal,publication FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3",params![id,sequence,authority],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            ["sealed", "failed", "published"].contains(&state.as_str()),
            "export is not accepted/sealed"
        );
        let sealed: SealedPhotoExport = serde_json::from_str(&encoded)?;
        ensure!(
            sealed.authority_digest == authority
                && sealed.snapshot == plan.destination
                && sealed.max_payload_bytes == plan.max_payload_bytes,
            "stored seal authority mismatch"
        );
        let mut session =
            metadata_export::PhotoPublication::prepare_with_checkpoint(&sealed, &mut |_| {
                hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
            })?;
        // Installed objects are finalized only under a pre-link committed intent.
        // A later edit/cancellation cannot turn that completed namespace action
        // into permission to rerender or republish a different object.
        let intent = if let Some(encoded) = previous {
            parse_intent(&encoded, &authority)?;
            encoded
        } else {
            serde_json::to_string(&PublicationIntent {
                version: 1,
                token: uuid::Uuid::new_v4().to_string(),
                authority: authority.clone(),
            })?
        };
        if session.installed() {
            let current: Option<String> = self.db.query_row(
                "SELECT publication FROM photo_export_items WHERE job=?1 AND sequence=?2",
                params![id, sequence],
                |r| r.get(0),
            )?;
            ensure!(
                current.as_deref() == Some(&intent),
                "installed payload has no committed intent"
            );
        } else {
            let start = std::time::Instant::now();
            let original = verify_original(&plan, &mut |_| {
                hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
            })?;
            metrics.original_hash_ms = start.elapsed().as_secs_f64() * 1000.;
            hook(PhotoExportBoundary::OriginalVerified)?;
            let start = std::time::Instant::now();
            self.with_edit_transaction(&plan.identity,Priority::Foreground,|tx|{
                metadata_current(tx,&plan)?;ensure!(job(tx,id)?.state=="queued","export canceled");original.recheck().context("original changed while waiting for export authority")?;
                protect_destination(tx,&plan.destination.destination,&plan.original.to_path()?,plan.alias_limits)?;
                ensure!(tx.execute("UPDATE photo_export_items SET publication=?1 WHERE job=?2 AND sequence=?3 AND authority=?4 AND state='sealed' AND (publication IS NULL OR publication=?1)",params![intent,id,sequence,authority])?==1,"export publication intent changed");Ok(())
            })?.context("edit/source changed before publication intent")?;
            metrics
                .authority_intervals_ms
                .push(start.elapsed().as_secs_f64() * 1000.);
            hook(PhotoExportBoundary::IntentCommitted)?;
            let start = std::time::Instant::now();
            let capture = self.with_edit_transaction(&plan.identity, Priority::Foreground, |tx| {
                publication_current(tx, id, sequence, &authority, &intent, &plan)?;
                original
                    .recheck()
                    .context("original changed while waiting for export authority")?;
                protect_destination(
                    tx,
                    &plan.destination.destination,
                    &plan.original.to_path()?,
                    plan.alias_limits,
                )?;
                session.capture()
            });
            metrics
                .authority_intervals_ms
                .push(start.elapsed().as_secs_f64() * 1000.);
            let capture = match capture {
                Ok(Some(())) => Ok(()),
                Ok(None) => Err(anyhow::anyhow!("edit/source changed before capture")),
                Err(error) => Err(error),
            };
            if let Err(error) = capture {
                let receipt = session.failure_receipt(format!("capture retained: {error:#}"));
                self.record_photo_publication_failure(id, sequence, &authority, &intent, &receipt)?;
                metrics.publication = session.timings().clone();
                metrics.total_ms = started.elapsed().as_secs_f64() * 1000.;
                return Ok((receipt, metrics));
            }
            hook(PhotoExportBoundary::Captured)?;
            session.verify_capture_with_checkpoint(&mut |_| {
                hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
            })?;
            hook(PhotoExportBoundary::CaptureVerified)?;
            let start = std::time::Instant::now();
            let link = self.with_edit_transaction(&plan.identity, Priority::Foreground, |tx| {
                publication_current(tx, id, sequence, &authority, &intent, &plan)?;
                original
                    .recheck()
                    .context("original changed while waiting for export authority")?;
                protect_destination(
                    tx,
                    &plan.destination.destination,
                    &plan.original.to_path()?,
                    plan.alias_limits,
                )?;
                session.link()
            });
            metrics
                .authority_intervals_ms
                .push(start.elapsed().as_secs_f64() * 1000.);
            match link {
                Ok(Some(())) => {}
                Ok(None) => anyhow::bail!("edit/source changed before publication link"),
                Err(error) => {
                    let receipt =
                        session.failure_receipt(format!("publication retained: {error:#}"));
                    self.record_photo_publication_failure(
                        id, sequence, &authority, &intent, &receipt,
                    )?;
                    metrics.publication = session.timings().clone();
                    metrics.total_ms = started.elapsed().as_secs_f64() * 1000.;
                    return Ok((receipt, metrics));
                }
            }
            hook(PhotoExportBoundary::Linked)?;
        }
        let receipt = session.verify_installed_with_checkpoint(&mut |_| {
            hook(PhotoExportBoundary::Hashing).map_err(std::io::Error::other)
        })?;
        hook(PhotoExportBoundary::InstalledVerified)?;
        let start = std::time::Instant::now();
        self.finalize_photo_publication(id, sequence, &authority, &intent, &session, &receipt)?;
        metrics
            .authority_intervals_ms
            .push(start.elapsed().as_secs_f64() * 1000.);
        metrics.publication = session.timings().clone();
        metrics.total_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok((receipt, metrics))
    }
    fn finalize_photo_publication(
        &mut self,
        id: &str,
        sequence: i64,
        authority: &str,
        intent: &str,
        session: &metadata_export::PhotoPublication,
        receipt: &ExportReceipt,
    ) -> Result<()> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state:String=tx.query_row("SELECT state FROM photo_export_items WHERE job=?1 AND sequence=?2 AND authority=?3 AND publication=?4",params![id,sequence,authority,intent],|r|r.get(0))?;
        ensure!(
            ["sealed", "failed", "published"].contains(&state.as_str()),
            "publication finalization ownership changed"
        );
        session.recheck_installed()?;
        tx.execute("UPDATE photo_export_items SET state='published',receipt=?1,error=NULL WHERE job=?2 AND sequence=?3",params![serde_json::to_string(receipt)?,id,sequence])?;
        if state == "sealed" {
            finish_item(&tx, id)?;
        }
        tx.commit()?;
        Ok(())
    }
    fn record_photo_publication_failure(
        &mut self,
        id: &str,
        sequence: i64,
        authority: &str,
        intent: &str,
        receipt: &ExportReceipt,
    ) -> Result<()> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(tx.execute("UPDATE photo_export_items SET state='failed',receipt=?1,error=?2 WHERE job=?3 AND sequence=?4 AND authority=?5 AND publication=?6 AND state='sealed'",params![serde_json::to_string(receipt)?,receipt.detail,id,sequence,authority,intent])?==1,"publication failure ownership changed");
        finish_item(&tx, id)?;
        tx.commit()?;
        Ok(())
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
