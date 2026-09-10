//! Durable independent edit variants. Recipes are immutable; undo moves a cursor
//! while every visible change advances a separate revision used by render guards.
use crate::{Catalog, catalog_metadata::RenderIdentity, catalog_writer::Priority, edit::Recipe};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

pub const MASTER: &str = "master";
const MAX_RECIPE_BYTES: usize = 64 * 1024;

pub(crate) const SCHEMA: &str = "
CREATE TABLE edit_variants(
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 asset_id TEXT NOT NULL REFERENCES assets(id), id TEXT NOT NULL,
 label TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>=0),
 cursor INTEGER REFERENCES edit_recipe_nodes(id), redo INTEGER REFERENCES edit_redo_nodes(id),
 UNIQUE(asset_id,id));
CREATE TABLE edit_recipe_nodes(
 id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL, variant_id TEXT NOT NULL,
 parent INTEGER REFERENCES edit_recipe_nodes(id), recipe BLOB NOT NULL, digest TEXT NOT NULL,
 FOREIGN KEY(asset_id,variant_id) REFERENCES edit_variants(asset_id,id));
CREATE INDEX edit_recipe_variant ON edit_recipe_nodes(asset_id,variant_id,id);
CREATE TABLE edit_redo_nodes(
 id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL, variant_id TEXT NOT NULL,
 recipe_node INTEGER NOT NULL REFERENCES edit_recipe_nodes(id),
 next INTEGER REFERENCES edit_redo_nodes(id),
 FOREIGN KEY(asset_id,variant_id) REFERENCES edit_variants(asset_id,id));
CREATE TABLE edit_changes(
 id INTEGER PRIMARY KEY, asset_id TEXT NOT NULL, variant_id TEXT NOT NULL,
 revision INTEGER NOT NULL, kind TEXT NOT NULL,
 previous_node INTEGER REFERENCES edit_recipe_nodes(id),
 current_node INTEGER NOT NULL REFERENCES edit_recipe_nodes(id),
 provenance TEXT NOT NULL,
 UNIQUE(asset_id,variant_id,revision),
 FOREIGN KEY(asset_id,variant_id) REFERENCES edit_variants(asset_id,id));
";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantKey {
    pub asset_id: String,
    pub variant_id: String,
}
impl VariantKey {
    pub fn master(asset: impl Into<String>) -> Self {
        Self { asset_id: asset.into(), variant_id: MASTER.into() }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.asset_id.is_empty() && self.asset_id.len() <= 256, "asset identity length");
        ensure!(!self.variant_id.is_empty() && self.variant_id.len() <= 256, "variant identity length");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantView {
    pub key: VariantKey,
    pub label: String,
    pub revision: i64,
    pub recipe: Recipe,
    pub recipe_digest: String,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditRenderIdentity {
    pub source: RenderIdentity,
    pub key: VariantKey,
    pub revision: i64,
    pub recipe_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditHistoryEntry {
    pub revision: i64,
    pub kind: String,
    pub recipe: Recipe,
    pub recipe_digest: String,
    pub provenance: serde_json::Value,
}

struct StoredVariant {
    label: String,
    revision: i64,
    cursor: i64,
    redo: Option<i64>,
}

fn canonical(recipe: &Recipe) -> Result<(Vec<u8>, String)> {
    let validated = recipe.validate()?;
    let bytes = validated.canonical_bytes().to_vec();
    ensure!(bytes.len() <= MAX_RECIPE_BYTES, "recipe exceeds durable document limit");
    Ok((bytes, validated.digest().to_owned()))
}

fn read_recipe(bytes: &[u8], digest: &str) -> Result<Recipe> {
    ensure!(bytes.len() <= MAX_RECIPE_BYTES, "stored recipe exceeds document limit");
    let recipe: Recipe = serde_json::from_slice(bytes)?;
    let (canonical, actual) = canonical(&recipe)?;
    ensure!(canonical == bytes && actual == digest, "stored recipe identity mismatch");
    Ok(recipe)
}

fn stored(db: &Connection, key: &VariantKey) -> Result<Option<StoredVariant>> {
    key.validate()?;
    Ok(db.query_row(
        "SELECT label,revision,cursor,redo FROM edit_variants WHERE asset_id=?1 AND id=?2",
        params![key.asset_id, key.variant_id],
        |r| Ok(StoredVariant { label: r.get(0)?, revision: r.get(1)?, cursor: r.get(2)?, redo: r.get(3)? }),
    ).optional()?)
}

fn asset_exists(db: &Connection, asset: &str) -> Result<()> {
    ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE id=?1)", [asset], |r| r.get::<_, bool>(0))?, "asset does not exist");
    Ok(())
}

fn view(db: &Connection, key: &VariantKey) -> Result<VariantView> {
    let Some(value) = stored(db, key)? else {
        ensure!(key.variant_id == MASTER, "variant does not exist");
        asset_exists(db, &key.asset_id)?;
        let recipe = Recipe::default();
        let (_, recipe_digest) = canonical(&recipe)?;
        return Ok(VariantView { key: key.clone(), label: "Original".into(), revision: 0, recipe, recipe_digest, can_undo: false, can_redo: false });
    };
    let (parent, bytes, digest): (Option<i64>, Vec<u8>, String) = db.query_row(
        "SELECT parent,recipe,digest FROM edit_recipe_nodes WHERE id=?1 AND asset_id=?2 AND variant_id=?3",
        params![value.cursor, key.asset_id, key.variant_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(VariantView { key: key.clone(), label: value.label, revision: value.revision, recipe: read_recipe(&bytes, &digest)?, recipe_digest: digest, can_undo: parent.is_some(), can_redo: value.redo.is_some() })
}

fn insert_variant(tx: &Transaction<'_>, key: &VariantKey, label: &str, recipe: &Recipe) -> Result<()> {
    key.validate()?;
    ensure!(!label.trim().is_empty() && label.len() <= 1024, "variant label length");
    let (bytes, digest) = canonical(recipe)?;
    tx.execute("INSERT INTO edit_variants(asset_id,id,label,revision) VALUES(?1,?2,?3,0)", params![key.asset_id, key.variant_id, label])?;
    tx.execute("INSERT INTO edit_recipe_nodes(asset_id,variant_id,parent,recipe,digest) VALUES(?1,?2,NULL,?3,?4)", params![key.asset_id, key.variant_id, bytes, digest])?;
    let node = tx.last_insert_rowid();
    tx.execute("UPDATE edit_variants SET cursor=?1 WHERE asset_id=?2 AND id=?3", params![node, key.asset_id, key.variant_id])?;
    Ok(())
}

fn materialize(tx: &Transaction<'_>, key: &VariantKey) -> Result<StoredVariant> {
    if let Some(value) = stored(tx, key)? { return Ok(value); }
    ensure!(key.variant_id == MASTER, "variant does not exist");
    asset_exists(tx, &key.asset_id)?;
    insert_variant(tx, key, "Original", &Recipe::default())?;
    stored(tx, key)?.context("new variant unavailable")
}

fn record_change(tx: &Transaction<'_>, key: &VariantKey, revision: i64, kind: &str, previous: i64, current: i64, provenance: &serde_json::Value) -> Result<()> {
    let provenance = serde_json::to_string(provenance)?;
    ensure!(provenance.len() <= MAX_RECIPE_BYTES, "edit provenance document limit");
    tx.execute("INSERT INTO edit_changes(asset_id,variant_id,revision,kind,previous_node,current_node,provenance) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![key.asset_id, key.variant_id, revision, kind, previous, current, provenance])?;
    Ok(())
}

fn save(tx: &Transaction<'_>, key: &VariantKey, expected: i64, bytes: &[u8], digest: &str, kind: &str, provenance: &serde_json::Value) -> Result<VariantView> {
    let current = materialize(tx, key)?;
    ensure!(expected >= 0 && current.revision == expected, "edit revision changed");
    let revision = current.revision.checked_add(1).context("edit revision exhausted")?;
    tx.execute("INSERT INTO edit_recipe_nodes(asset_id,variant_id,parent,recipe,digest) VALUES(?1,?2,?3,?4,?5)", params![key.asset_id, key.variant_id, current.cursor, bytes, digest])?;
    let node = tx.last_insert_rowid();
    // Clearing one pointer invalidates an arbitrarily deep redo chain in O(1).
    // Immutable nodes remain evidence and are included by catalog backup.
    tx.execute("UPDATE edit_variants SET revision=?1,cursor=?2,redo=NULL WHERE asset_id=?3 AND id=?4", params![revision, node, key.asset_id, key.variant_id])?;
    record_change(tx, key, revision, kind, current.cursor, node, provenance)?;
    view(tx, key)
}

impl Catalog {
    pub fn edit_variant(&self, key: &VariantKey) -> Result<VariantView> {
        let tx = self.db.unchecked_transaction()?;
        let value = view(&tx, key)?;
        tx.commit()?;
        Ok(value)
    }

    /// The implicit master is always returned separately by edit_variant(master).
    /// This page lists only persisted variants and never scans the asset table.
    pub fn edit_variants(&self, asset: &str, after: i64, limit: usize) -> Result<Vec<(i64, VariantView)>> {
        ensure!((1..=200).contains(&limit) && after >= 0, "variant page bounds");
        let tx = self.db.unchecked_transaction()?;
        asset_exists(&tx, asset)?;
        let keys = tx.prepare("SELECT sequence,id FROM edit_variants WHERE asset_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")?.query_map(params![asset, after, limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let result = keys.into_iter().map(|(sequence, variant_id)| Ok((sequence, view(&tx, &VariantKey { asset_id: asset.into(), variant_id })?))).collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(result)
    }

    pub fn create_edit_variant(&mut self, source: &VariantKey, expected_revision: i64, label: &str) -> Result<VariantView> {
        // Validation before writer admission keeps expensive interpretation out of
        // the transaction. The immutable source is checked again inside it.
        let input = self.edit_variant(source)?;
        ensure!(input.revision == expected_revision, "source edit revision changed");
        canonical(&input.recipe)?;
        let key = VariantKey { asset_id: source.asset_id.clone(), variant_id: uuid::Uuid::new_v4().to_string() };
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = view(&tx, source)?;
        ensure!(current.revision == expected_revision && current.recipe_digest == input.recipe_digest, "source edit revision changed");
        insert_variant(&tx, &key, label, &input.recipe)?;
        let value = stored(&tx, &key)?.context("new variant unavailable")?;
        record_change(&tx, &key, 0, "create", value.cursor, value.cursor, &serde_json::json!({"source":source,"source_revision":expected_revision,"recipe_digest":input.recipe_digest}))?;
        let result = view(&tx, &key)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn save_edit_recipe(&mut self, key: &VariantKey, expected_revision: i64, recipe: &Recipe) -> Result<VariantView> {
        key.validate()?;
        let (bytes, digest) = canonical(recipe)?;
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = save(&tx, key, expected_revision, &bytes, &digest, "save", &serde_json::json!({}))?;
        tx.commit()?;
        Ok(result)
    }

    pub fn undo_edit(&mut self, key: &VariantKey, expected_revision: i64) -> Result<VariantView> {
        self.move_edit_cursor(key, expected_revision, false)
    }

    pub fn redo_edit(&mut self, key: &VariantKey, expected_revision: i64) -> Result<VariantView> {
        self.move_edit_cursor(key, expected_revision, true)
    }

    fn move_edit_cursor(&mut self, key: &VariantKey, expected: i64, redo: bool) -> Result<VariantView> {
        key.validate()?;
        let _write = self.writers.enter(Priority::Foreground)?;
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = materialize(&tx, key)?;
        ensure!(expected >= 0 && current.revision == expected, "edit revision changed");
        let revision = current.revision.checked_add(1).context("edit revision exhausted")?;
        let (node, next_redo) = if redo {
            let top = current.redo.context("no edit to redo")?;
            let (node, next): (i64, Option<i64>) = tx.query_row("SELECT recipe_node,next FROM edit_redo_nodes WHERE id=?1 AND asset_id=?2 AND variant_id=?3", params![top,key.asset_id,key.variant_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
            let parent: Option<i64> = tx.query_row("SELECT parent FROM edit_recipe_nodes WHERE id=?1 AND asset_id=?2 AND variant_id=?3", params![node,key.asset_id,key.variant_id], |r|r.get(0))?;
            ensure!(parent == Some(current.cursor), "redo lineage does not match current recipe");
            (node, next)
        } else {
            let parent: Option<i64> = tx.query_row("SELECT parent FROM edit_recipe_nodes WHERE id=?1 AND asset_id=?2 AND variant_id=?3", params![current.cursor,key.asset_id,key.variant_id], |r|r.get(0))?;
            let parent = parent.context("no edit to undo")?;
            tx.execute("INSERT INTO edit_redo_nodes(asset_id,variant_id,recipe_node,next) VALUES(?1,?2,?3,?4)", params![key.asset_id,key.variant_id,current.cursor,current.redo])?;
            (parent, Some(tx.last_insert_rowid()))
        };
        tx.execute("UPDATE edit_variants SET revision=?1,cursor=?2,redo=?3 WHERE asset_id=?4 AND id=?5", params![revision,node,next_redo,key.asset_id,key.variant_id])?;
        record_change(&tx, key, revision, if redo {"redo"} else {"undo"}, current.cursor, node, &serde_json::json!({}))?;
        let result = view(&tx, key)?;
        tx.commit()?;
        Ok(result)
    }

    pub fn edit_history(&self, key: &VariantKey, after_revision: i64, limit: usize) -> Result<Vec<EditHistoryEntry>> {
        key.validate()?;
        ensure!((1..=200).contains(&limit) && after_revision >= -1, "edit history page bounds");
        self.db.prepare("SELECT c.revision,c.kind,n.recipe,n.digest,c.provenance FROM edit_changes c JOIN edit_recipe_nodes n ON n.id=c.current_node WHERE c.asset_id=?1 AND c.variant_id=?2 AND c.revision>?3 ORDER BY c.revision LIMIT ?4")?.query_map(params![key.asset_id,key.variant_id,after_revision,limit as i64], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?.map(|row| {
            let (revision,kind,bytes,recipe_digest,provenance)=row?;
            Ok(EditHistoryEntry {revision,kind,recipe:read_recipe(&bytes,&recipe_digest)?,recipe_digest,provenance:serde_json::from_str(&provenance)?})
        }).collect()
    }

    pub fn edit_render_identity(&self, key: &VariantKey) -> Result<EditRenderIdentity> {
        let tx = self.db.unchecked_transaction()?;
        let variant = view(&tx, key)?;
        let source = tx.query_row("SELECT a.render_generation,a.fingerprint,a.state,COALESCE(m.revision,0) FROM assets a LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE a.id=?1", [&key.asset_id], |r| Ok(RenderIdentity {asset_id:key.asset_id.clone(),generation:r.get(0)?,fingerprint:r.get(1)?,state:r.get(2)?,metadata_revision:r.get(3)?}))?;
        tx.commit()?;
        Ok(EditRenderIdentity {source,key:key.clone(),revision:variant.revision,recipe_digest:variant.recipe_digest})
    }

    pub fn with_edit_identity<T>(&mut self, expected: &EditRenderIdentity, attach: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
        self.with_edit_transaction(expected, Priority::Foreground, |_| attach())
    }

    pub(crate) fn with_edit_transaction<T>(&mut self, expected: &EditRenderIdentity, priority: Priority, attach: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<Option<T>> {
        expected.key.validate()?;
        ensure!(expected.key.asset_id == expected.source.asset_id, "mixed edit/source identities");
        Ok(self.with_render_transaction(&expected.source, priority, |tx| {
            let current = view(tx, &expected.key)?;
            if current.revision != expected.revision || current.recipe_digest != expected.recipe_digest { return Ok(None); }
            Ok(Some(attach(tx)?))
        })?.flatten())
    }
}
