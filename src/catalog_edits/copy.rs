use super::*;
use crate::edit::AdjustmentGroup;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditTarget {
    pub key: VariantKey,
    pub expected_revision: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyJob {
    pub sequence: i64,
    pub id: String,
    pub state: String,
    pub total: i64,
    pub completed: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyItem {
    pub sequence: i64,
    pub target: EditTarget,
    pub state: String,
    pub applied_revision: Option<i64>,
    pub error: Option<String>,
}

fn job(db: &Connection, id: &str) -> Result<CopyJob> {
    Ok(db.query_row(
        "SELECT state,total,completed,sequence FROM edit_copy_jobs WHERE id=?1",
        [id],
        |r| {
            Ok(CopyJob {
                id: id.into(),
                state: r.get(0)?,
                total: r.get(1)?,
                completed: r.get(2)?,
                sequence: r.get(3)?,
            })
        },
    )?)
}

impl Catalog {
    /// Freeze source settings now, then append bounded target pages and seal.
    /// No asset-count ceiling is imposed on a job; no full-library Vec is needed.
    pub fn begin_edit_copy(
        &mut self,
        source: &VariantKey,
        expected_revision: i64,
        groups: &[AdjustmentGroup],
    ) -> Result<CopyJob> {
        ensure!(
            !groups.is_empty() && groups.len() <= 7,
            "select one to seven adjustment groups"
        );
        let source_view = self.edit_variant(source)?;
        ensure!(
            source_view.revision == expected_revision,
            "source edit revision changed"
        );
        let (recipe, digest) = canonical(&source_view.recipe)?;
        let source_json = serde_json::to_string(
            &serde_json::json!({"key":source,"revision":expected_revision,"digest":digest}),
        )?;
        let groups_json = serde_json::to_string(groups)?;
        let id = uuid::Uuid::new_v4().to_string();
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = view(&tx, source)?;
        ensure!(
            current.revision == expected_revision && current.recipe_digest == digest,
            "source edit revision changed"
        );
        tx.execute("INSERT INTO edit_copy_jobs(id,source,recipe,digest,groups_json,state) VALUES(?1,?2,?3,?4,?5,'building')", params![id,source_json,recipe,digest,groups_json])?;
        let result = job(&tx, &id)?;
        tx.commit()?;
        Ok(result)
    }

    /// Expected total makes target-page retries safe and prevents accidental
    /// duplication after the caller loses a response. A retried old page is an
    /// explicit conflict; read job/items to reconcile before appending again.
    pub fn append_edit_copy(
        &mut self,
        id: &str,
        expected_total: i64,
        targets: &[EditTarget],
    ) -> Result<CopyJob> {
        ensure!(
            !targets.is_empty() && targets.len() <= 200,
            "copy target page bounds"
        );
        for target in targets {
            target.key.validate()?;
            ensure!(target.expected_revision >= 0, "negative edit revision");
        }
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = job(&tx, id)?;
        ensure!(
            current.state == "building" && current.total == expected_total,
            "copy target list changed or sealed"
        );
        let total = current
            .total
            .checked_add(targets.len() as i64)
            .context("copy target count exhausted")?;
        for (index, target) in targets.iter().enumerate() {
            tx.execute("INSERT INTO edit_copy_items(job,sequence,asset_id,variant_id,expected_revision,state) VALUES(?1,?2,?3,?4,?5,'pending')",params![id,current.total+index as i64+1,target.key.asset_id,target.key.variant_id,target.expected_revision])?;
        }
        tx.execute(
            "UPDATE edit_copy_jobs SET total=?1 WHERE id=?2",
            params![total, id],
        )?;
        let result = job(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn seal_edit_copy(&mut self, id: &str, expected_total: i64) -> Result<CopyJob> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = job(&tx, id)?;
        ensure!(
            current.state == "building" && current.total == expected_total,
            "copy target list changed or already sealed"
        );
        tx.execute(
            "UPDATE edit_copy_jobs SET state=?1 WHERE id=?2",
            params![
                if current.total == 0 {
                    "complete"
                } else {
                    "queued"
                },
                id
            ],
        )?;
        let result = job(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn cancel_edit_copy(&mut self, id: &str) -> Result<CopyJob> {
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE edit_copy_jobs SET state='canceled' WHERE id=?1 AND state IN ('building','queued')",[id])?;
        let result = job(&tx, id)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn edit_copy_job(&self, id: &str) -> Result<CopyJob> {
        job(&self.db, id)
    }

    pub fn edit_copy_jobs(&self, after: i64, limit: usize) -> Result<Vec<CopyJob>> {
        ensure!(
            after >= 0 && (1..=200).contains(&limit),
            "copy job page bounds"
        );
        Ok(self.db.prepare("SELECT sequence,id,state,total,completed FROM edit_copy_jobs WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?.query_map(params![after,limit as i64],|r|Ok(CopyJob{sequence:r.get(0)?,id:r.get(1)?,state:r.get(2)?,total:r.get(3)?,completed:r.get(4)?}))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn edit_copy_items(&self, id: &str, after: i64, limit: usize) -> Result<Vec<CopyItem>> {
        ensure!(
            after >= 0 && (1..=200).contains(&limit),
            "copy item page bounds"
        );
        Ok(self.db.prepare("SELECT sequence,asset_id,variant_id,expected_revision,state,applied_revision,error FROM edit_copy_items WHERE job=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?.query_map(params![id,after,limit as i64],|r|Ok(CopyItem {sequence:r.get(0)?,target:EditTarget {key:VariantKey {asset_id:r.get(1)?,variant_id:r.get(2)?},expected_revision:r.get(3)?},state:r.get(4)?,applied_revision:r.get(5)?,error:r.get(6)?}))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Each target result and its edit are one transaction. Cancellation and
    /// foreground writes can run between targets; a crash cannot lose progress
    /// after applying an edit or apply the same target a second time.
    pub fn apply_edit_copy_step(&mut self, id: &str, limit: usize) -> Result<CopyJob> {
        ensure!((1..=100).contains(&limit), "copy work bounds");
        let current = job(&self.db, id)?;
        if current.state == "complete" || current.state == "canceled" {
            return Ok(current);
        }
        ensure!(current.state == "queued", "copy job is not sealed");
        let (source, bytes, digest, groups): (String, Vec<u8>, String, String) =
            self.db.query_row(
                "SELECT source,recipe,digest,groups_json FROM edit_copy_jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let recipe = read_recipe(&bytes, &digest)?;
        let groups: Vec<AdjustmentGroup> = serde_json::from_str(&groups)?;
        let sequences=self.db.prepare("SELECT sequence FROM edit_copy_items WHERE job=?1 AND state='pending' ORDER BY sequence LIMIT ?2")?.query_map(params![id,limit as i64],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for sequence in sequences {
            let item = self
                .edit_copy_items(id, sequence - 1, 1)?
                .into_iter()
                .next()
                .context("copy item missing")?;
            let _write = self.writers.enter(Priority::Foreground)?;
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if job(&tx, id)?.state != "queued" {
                tx.commit()?;
                break;
            }
            let state: String = tx.query_row(
                "SELECT state FROM edit_copy_items WHERE job=?1 AND sequence=?2",
                params![id, sequence],
                |r| r.get(0),
            )?;
            if state != "pending" {
                tx.commit()?;
                continue;
            }
            let exists: bool = if item.target.key.variant_id == MASTER {
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE id=?1)",
                    [&item.target.key.asset_id],
                    |r| r.get(0),
                )?
            } else {
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM edit_variants WHERE asset_id=?1 AND id=?2)",
                    params![item.target.key.asset_id, item.target.key.variant_id],
                    |r| r.get(0),
                )?
            };
            let prepared: std::result::Result<(Vec<u8>, String), (&str, String)> = if !exists {
                Err(("incompatible", "asset or variant unavailable".into()))
            } else {
                let target = view(&tx, &item.target.key)?;
                if target.revision != item.target.expected_revision {
                    Err(("conflict", "edit revision changed".into()))
                } else {
                    match target.recipe.copy_groups_from(&recipe, &groups) {
                        Err(error) => Err(("incompatible", error.to_string())),
                        Ok(merged) => {
                            let dimensions = known_dimensions(&tx, &item.target.key.asset_id)?;
                            let checked = dimensions
                                .map(|(w, h)| merged.validate_dimensions(w, h))
                                .transpose();
                            match checked {
                                Err(error) => Err(("incompatible", error.to_string())),
                                Ok(_) => Ok((
                                    merged.canonical_bytes().to_vec(),
                                    merged.digest().to_owned(),
                                )),
                            }
                        }
                    }
                }
            };
            let result = match prepared {
                Ok((bytes, digest)) => {
                    let current = view(&tx, &item.target.key)?;
                    if current.revision != item.target.expected_revision {
                        Err(("conflict", "edit revision changed".into()))
                    } else {
                        // Operational/database failures propagate and roll back;
                        // they are never misreported as incompatible adjustments.
                        let value = save(
                            &tx,
                            &item.target.key,
                            item.target.expected_revision,
                            &bytes,
                            &digest,
                            "copy",
                            &serde_json::json!({"copy_job":id,"source":serde_json::from_str::<serde_json::Value>(&source)?}),
                        )?;
                        Ok(value.revision)
                    }
                }
                Err(error) => Err(error),
            };
            let (state, revision, error) = match result {
                Ok(revision) => ("applied", Some(revision), None),
                Err((state, error)) => (
                    state,
                    None,
                    Some(error.chars().take(2048).collect::<String>()),
                ),
            };
            tx.execute("UPDATE edit_copy_items SET state=?1,applied_revision=?2,error=?3 WHERE job=?4 AND sequence=?5",params![state,revision,error,id,sequence])?;
            tx.execute("UPDATE edit_copy_jobs SET completed=completed+1,state=CASE WHEN completed+1=total THEN 'complete' ELSE state END WHERE id=?1",[id])?;
            tx.commit()?;
        }
        job(&self.db, id)
    }
}
