//! Logical images share physical storage but own metadata and organization state.
use crate::{
    Catalog,
    catalog_edits::{MASTER, VariantKey},
    catalog_writer::Priority,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRole {
    Master,
    Virtual,
    NativeCopy,
}
impl ImageRole {
    fn sql(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Virtual => "virtual",
            Self::NativeCopy => "native_copy",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationState {
    Native,
    Translated,
    RetainedOnly,
    Untranslated,
}
impl TranslationState {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Translated => "translated",
            Self::RetainedOnly => "retained_only",
            Self::Untranslated => "untranslated",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub sequence: i64,
    pub key: VariantKey,
    pub role: ImageRole,
    pub origin: String,
    pub master_sequence: Option<i64>,
    pub copied_from_sequence: Option<i64>,
    pub translation_state: String,
}
/// New image authorities are separate from legacy asset render generations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageMetadataIdentity {
    pub image_id: String,
    pub key: VariantKey,
    pub metadata_revision: i64,
    pub pixel_generation: i64,
    pub shared_source_epoch: i64,
    pub physical_generation: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportImageRequest {
    pub import_source: String,
    pub capture_revision: String,
    pub source_table: String,
    pub source_id: String,
    pub input_digest: String,
    pub adapter_version: String,
    pub asset_id: String,
    pub claim_reserved_master: bool,
    pub role: ImageRole,
    pub master: Option<VariantKey>,
    pub label: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ImageRefreshProgress {
    pub processed: usize,
    pub pending: bool,
}

pub mod organization;

const SCHEMA: &str = r#"
ALTER TABLE assets ADD COLUMN physical_generation INTEGER NOT NULL DEFAULT 0;
UPDATE assets SET physical_generation=render_generation;
CREATE TABLE catalog_images(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,
 asset_id TEXT NOT NULL REFERENCES assets(id),variant_id TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN('master','virtual','native_copy')),origin TEXT NOT NULL CHECK(origin IN('native','import')),
 master_sequence INTEGER,copied_from_sequence INTEGER,translation_state TEXT NOT NULL CHECK(translation_state IN('native','translated','retained_only','untranslated')),
 pixel_generation INTEGER NOT NULL DEFAULT 0,applied_shared_epoch INTEGER NOT NULL DEFAULT 0,
 UNIQUE(asset_id,variant_id),UNIQUE(sequence,asset_id),
 FOREIGN KEY(master_sequence,asset_id) REFERENCES catalog_images(sequence,asset_id),
 FOREIGN KEY(copied_from_sequence,asset_id) REFERENCES catalog_images(sequence,asset_id),
 CHECK((role='virtual' AND master_sequence IS NOT NULL) OR (role!='virtual' AND master_sequence IS NULL)),
 CHECK(variant_id!='master' OR (role='master' AND id=asset_id)),
 CHECK(master_sequence IS NULL OR master_sequence!=sequence),
 CHECK(copied_from_sequence IS NULL OR copied_from_sequence<sequence));
CREATE INDEX catalog_images_asset ON catalog_images(asset_id,sequence);
CREATE TABLE image_import_reservations(asset_id TEXT PRIMARY KEY REFERENCES assets(id) DEFERRABLE INITIALLY DEFERRED,owner TEXT NOT NULL);
CREATE TABLE image_storage_events(asset_id TEXT PRIMARY KEY REFERENCES assets(id),high_water INTEGER NOT NULL,cursor INTEGER NOT NULL DEFAULT 0);
INSERT INTO catalog_images(sequence,id,asset_id,variant_id,role,origin,translation_state) SELECT sequence,id,id,'master','master','native','native' FROM assets ORDER BY sequence;
INSERT INTO catalog_images(id,asset_id,variant_id,role,origin,translation_state) SELECT 'variant:'||asset_id||':'||id,asset_id,id,'native_copy','native','native' FROM edit_variants WHERE id!='master' ORDER BY sequence;
CREATE TABLE image_import_map(import_source TEXT NOT NULL,capture_revision TEXT NOT NULL,source_table TEXT NOT NULL,source_id TEXT NOT NULL,input_digest TEXT NOT NULL,adapter_version TEXT NOT NULL,image_id TEXT NOT NULL REFERENCES catalog_images(id),PRIMARY KEY(import_source,capture_revision,source_table,source_id));
CREATE UNIQUE INDEX image_import_once ON image_import_map(image_id);
CREATE TABLE metadata_image_sources(image_id TEXT NOT NULL REFERENCES catalog_images(id),source_id INTEGER NOT NULL REFERENCES metadata_sources(id),current_observation INTEGER REFERENCES metadata_observations(id),follow_file INTEGER NOT NULL CHECK(follow_file IN(0,1)),logical_locator BLOB NOT NULL,association TEXT NOT NULL,availability TEXT NOT NULL,PRIMARY KEY(image_id,source_id));
CREATE INDEX metadata_image_source_follow ON metadata_image_sources(source_id,image_id);
CREATE TABLE metadata_image_observations(image_id TEXT NOT NULL REFERENCES catalog_images(id),observation_id INTEGER NOT NULL REFERENCES metadata_observations(id),PRIMARY KEY(image_id,observation_id));
CREATE TABLE image_shared_state(asset_id TEXT PRIMARY KEY REFERENCES assets(id),epoch INTEGER NOT NULL DEFAULT 0);
INSERT INTO image_shared_state SELECT id,0 FROM assets;
CREATE TABLE image_shared_events(id INTEGER PRIMARY KEY,asset_id TEXT NOT NULL REFERENCES assets(id),epoch INTEGER NOT NULL,source_id INTEGER NOT NULL REFERENCES metadata_sources(id),observation_id INTEGER REFERENCES metadata_observations(id),current_observation INTEGER REFERENCES metadata_observations(id),association TEXT NOT NULL,availability TEXT NOT NULL,high_water INTEGER NOT NULL,cursor INTEGER NOT NULL DEFAULT 0,UNIQUE(asset_id,epoch));
CREATE INDEX image_shared_event_asset ON image_shared_events(asset_id,epoch);
INSERT INTO metadata_image_sources SELECT i.id,s.id,s.current_observation,s.kind IN('sidecar','embedded'),s.locator,s.association,s.availability FROM catalog_images i JOIN metadata_sources s ON s.asset_id=i.asset_id;
INSERT INTO metadata_image_observations SELECT i.id,o.id FROM catalog_images i JOIN metadata_sources s ON s.asset_id=i.asset_id JOIN metadata_observations o ON o.source_id=s.id;
CREATE VIEW image_assets AS SELECT i.sequence,i.id,a.location,a.path_display,a.fingerprint,a.state,a.metadata,a.preview_hash,a.error,a.render_generation,i.asset_id,i.variant_id FROM catalog_images i JOIN assets a ON a.id=i.asset_id;
CREATE VIEW image_metadata_sources AS SELECT s.id,a.image_id AS asset_id,s.kind,a.logical_locator AS locator,s.display,a.association,a.availability,a.current_observation FROM metadata_image_sources a JOIN metadata_sources s ON s.id=a.source_id;
CREATE TRIGGER image_parent_insert BEFORE INSERT ON catalog_images WHEN new.master_sequence IS NOT NULL BEGIN SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM catalog_images WHERE sequence=new.master_sequence AND asset_id=new.asset_id AND role='master') THEN RAISE(ABORT,'virtual parent must be a same-asset master') END; END;
CREATE TRIGGER image_identity_immutable BEFORE UPDATE OF id,sequence,asset_id,variant_id,role,master_sequence,copied_from_sequence ON catalog_images BEGIN SELECT RAISE(ABORT,'logical image identity and ancestry are immutable'); END;
CREATE TRIGGER image_edit_insert BEFORE INSERT ON edit_variants BEGIN SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM catalog_images WHERE asset_id=new.asset_id AND variant_id=new.id) THEN RAISE(ABORT,'edit variant needs registered image') END; END;
CREATE TRIGGER image_edit_delete BEFORE DELETE ON catalog_images WHEN EXISTS(SELECT 1 FROM edit_variants WHERE asset_id=old.asset_id AND id=old.variant_id) BEGIN SELECT RAISE(ABORT,'image has edit history'); END;
CREATE TRIGGER image_association_insert BEFORE INSERT ON metadata_image_sources BEGIN
 SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM catalog_images i JOIN metadata_sources s ON s.asset_id=i.asset_id WHERE i.id=new.image_id AND s.id=new.source_id) THEN RAISE(ABORT,'source belongs to another physical asset') END;
 SELECT CASE WHEN new.current_observation IS NOT NULL AND NOT EXISTS(SELECT 1 FROM metadata_observations o JOIN metadata_image_observations h ON h.observation_id=o.id WHERE o.id=new.current_observation AND o.source_id=new.source_id AND h.image_id=new.image_id) THEN RAISE(ABORT,'current observation needs image history membership') END;
END;
CREATE TRIGGER image_association_update BEFORE UPDATE ON metadata_image_sources BEGIN
 SELECT CASE WHEN new.image_id!=old.image_id OR new.source_id!=old.source_id THEN RAISE(ABORT,'association identity is immutable') END;
 SELECT CASE WHEN new.current_observation IS NOT NULL AND NOT EXISTS(SELECT 1 FROM metadata_observations o JOIN metadata_image_observations h ON h.observation_id=o.id WHERE o.id=new.current_observation AND o.source_id=new.source_id AND h.image_id=new.image_id) THEN RAISE(ABORT,'current observation needs image history membership') END;
END;
CREATE TRIGGER image_history_insert BEFORE INSERT ON metadata_image_observations BEGIN SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM catalog_images i JOIN metadata_sources s ON s.asset_id=i.asset_id JOIN metadata_observations o ON o.source_id=s.id WHERE i.id=new.image_id AND o.id=new.observation_id) THEN RAISE(ABORT,'observation belongs to another physical asset') END; END;
CREATE TRIGGER image_history_update BEFORE UPDATE ON metadata_image_observations BEGIN SELECT RAISE(ABORT,'observation membership is immutable'); END;
CREATE TRIGGER image_history_delete BEFORE DELETE ON metadata_image_observations BEGIN SELECT RAISE(ABORT,'observation membership is immutable'); END;
"#;

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    // Rebuild only leaf metadata/organization tables. Cyclic edit tables and their IDs stay intact.
    let names = [
        "metadata_assets",
        "metadata_choices",
        "metadata_history",
        "metadata_effective",
        "organization_dirty",
        "organization_folder_members",
        "organization_keyword_members",
        "organization_collection_members",
        "organization_flags",
        "organization_assets",
        "organization_job_items",
        "organization_events",
    ];
    let triggers=db.prepare("SELECT name,sql FROM sqlite_master WHERE type='trigger' AND name LIKE 'organization_%'")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, _) in &triggers {
        db.execute_batch(&format!("DROP TRIGGER {name}"))?;
    }
    for name in names {
        let sql: String = db.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |r| r.get(0),
        )?;
        let indices=db.prepare("SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name=? AND sql IS NOT NULL")?.query_map([name],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let body = sql.find('(').context("table definition lacks columns")?;
        let replacement = format!("CREATE TABLE {name}_image_new {}", &sql[body..])
            .replace("REFERENCES assets(id)", "REFERENCES catalog_images(id)")
            .replace(
                "REFERENCES assets(sequence)",
                "REFERENCES catalog_images(sequence)",
            );
        db.execute_batch(&replacement)?;
        db.execute_batch(&format!("INSERT INTO {name}_image_new SELECT * FROM {name}; DROP TABLE {name}; ALTER TABLE {name}_image_new RENAME TO {name};"))?;
        for index in indices {
            db.execute_batch(&index)?;
        }
    }
    db.execute_batch("INSERT INTO metadata_assets SELECT i.id,COALESCE(m.revision,0) FROM catalog_images i LEFT JOIN metadata_assets m ON m.asset_id=i.asset_id WHERE i.variant_id!='master'; INSERT INTO metadata_choices SELECT i.id,c.field,c.model_id FROM catalog_images i JOIN metadata_choices c ON c.asset_id=i.asset_id WHERE i.variant_id!='master'; INSERT INTO metadata_effective SELECT i.id,e.field,e.value,e.conflicted,e.model_id FROM catalog_images i JOIN metadata_effective e ON e.asset_id=i.asset_id WHERE i.variant_id!='master'; INSERT OR IGNORE INTO organization_dirty SELECT sequence FROM catalog_images WHERE variant_id!='master'; UPDATE organization_state SET backfill_high=MAX(backfill_high,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE variant_id!='master'));")?;
    db.execute_batch(organization::SCHEMA)?;
    db.execute_batch(r#"
CREATE TRIGGER image_asset_insert AFTER INSERT ON assets BEGIN
 INSERT INTO catalog_images(id,asset_id,variant_id,role,origin,translation_state) VALUES(new.id,new.id,'master','master','native','native');
 INSERT INTO image_shared_state VALUES(new.id,0);
 INSERT INTO organization_dirty SELECT sequence FROM catalog_images WHERE id=new.id AND NOT EXISTS(SELECT 1 FROM organization_dirty d WHERE d.sequence=catalog_images.sequence);
END;
CREATE TRIGGER image_asset_update AFTER UPDATE OF metadata,path_display,state,location,fingerprint ON assets BEGIN UPDATE assets SET physical_generation=physical_generation+1 WHERE id=new.id; UPDATE image_storage_events SET high_water=(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.id),cursor=0 WHERE asset_id=new.id; INSERT INTO image_storage_events SELECT new.id,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.id),0 WHERE NOT EXISTS(SELECT 1 FROM image_storage_events WHERE asset_id=new.id); END;
CREATE TRIGGER image_binding_insert AFTER INSERT ON storage_bindings BEGIN UPDATE assets SET physical_generation=physical_generation+1 WHERE id=new.asset_id; UPDATE image_storage_events SET high_water=(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.asset_id),cursor=0 WHERE asset_id=new.asset_id; INSERT INTO image_storage_events SELECT new.asset_id,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.asset_id),0 WHERE NOT EXISTS(SELECT 1 FROM image_storage_events WHERE asset_id=new.asset_id); END;
CREATE TRIGGER image_binding_update AFTER UPDATE ON storage_bindings BEGIN UPDATE assets SET physical_generation=physical_generation+1 WHERE id=new.asset_id; UPDATE image_storage_events SET high_water=(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.asset_id),cursor=0 WHERE asset_id=new.asset_id; INSERT INTO image_storage_events SELECT new.asset_id,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=new.asset_id),0 WHERE NOT EXISTS(SELECT 1 FROM image_storage_events WHERE asset_id=new.asset_id); END;
CREATE TRIGGER organization_metadata_update AFTER UPDATE ON metadata_assets BEGIN INSERT INTO organization_dirty SELECT sequence FROM catalog_images WHERE id=new.asset_id AND NOT EXISTS(SELECT 1 FROM organization_dirty d WHERE d.sequence=catalog_images.sequence); END;
"#)?;
    Ok(())
}

pub(crate) fn id(db: &Connection, key: &VariantKey) -> Result<String> {
    key.validate()?;
    db.query_row(
        "SELECT id FROM catalog_images WHERE asset_id=?1 AND variant_id=?2",
        params![key.asset_id, key.variant_id],
        |r| r.get(0),
    )
    .context("logical image not found")
}
pub(crate) fn physical(db: &Connection, image: &str) -> Result<String> {
    db.query_row(
        "SELECT asset_id FROM catalog_images WHERE id=?",
        [image],
        |r| r.get(0),
    )
    .context("logical image not found")
}
pub(crate) fn require_current(db: &Connection, image: &str) -> Result<()> {
    ensure!(db.query_row("SELECT i.applied_shared_epoch=s.epoch FROM catalog_images i JOIN image_shared_state s ON s.asset_id=i.asset_id WHERE i.id=?",[image],|r|r.get::<_,bool>(0))?,"image metadata pending shared-source refresh");
    Ok(())
}
pub(crate) fn identity(db: &Connection, image: &str) -> Result<ImageMetadataIdentity> {
    require_current(db, image)?;
    Ok(db.query_row("SELECT i.id,i.asset_id,i.variant_id,COALESCE(m.revision,0),i.pixel_generation,s.epoch,a.physical_generation FROM catalog_images i JOIN assets a ON a.id=i.asset_id JOIN image_shared_state s ON s.asset_id=i.asset_id LEFT JOIN metadata_assets m ON m.asset_id=i.id WHERE i.id=?",[image],|r|Ok(ImageMetadataIdentity{image_id:r.get(0)?,key:VariantKey{asset_id:r.get(1)?,variant_id:r.get(2)?},metadata_revision:r.get(3)?,pixel_generation:r.get(4)?,shared_source_epoch:r.get(5)?,physical_generation:r.get(6)?}))?)
}
/// Call inside the same writer transaction as authority reservation/publication.
pub fn require_image_metadata_identity(
    db: &Connection,
    expected: &ImageMetadataIdentity,
) -> Result<()> {
    ensure!(
        identity(db, &expected.image_id)? == *expected,
        "image metadata/source identity changed"
    );
    Ok(())
}
fn read_image(db: &Connection, image: &str) -> Result<Image> {
    let (id,sequence,asset,variant,role,origin,master,copied,translation):(String,i64,String,String,String,String,Option<i64>,Option<i64>,String)=db.query_row("SELECT id,sequence,asset_id,variant_id,role,origin,master_sequence,copied_from_sequence,translation_state FROM catalog_images WHERE id=?",[image],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?;
    Ok(Image {
        id,
        sequence,
        key: VariantKey {
            asset_id: asset,
            variant_id: variant,
        },
        role: match role.as_str() {
            "master" => ImageRole::Master,
            "virtual" => ImageRole::Virtual,
            _ => ImageRole::NativeCopy,
        },
        origin,
        master_sequence: master,
        copied_from_sequence: copied,
        translation_state: translation,
    })
}
/// Registers a native copy before recipe insertion; copied historical access is a fixed snapshot.
pub(crate) fn register_copy(db: &Connection, key: &VariantKey, source: &VariantKey) -> Result<()> {
    let from = id(db, source)?;
    require_current(db, &from)?;
    db.execute("INSERT INTO catalog_images(id,asset_id,variant_id,role,origin,copied_from_sequence,translation_state,applied_shared_epoch) SELECT ?1,asset_id,?2,'native_copy','native',sequence,'native',applied_shared_epoch FROM catalog_images WHERE id=?3",params![uuid::Uuid::new_v4().to_string(),key.variant_id,from])?;
    let target = id(db, key)?;
    db.execute("INSERT INTO metadata_image_observations SELECT ?1,observation_id FROM metadata_image_observations WHERE image_id=?2",params![target,from])?;
    db.execute("INSERT INTO metadata_image_sources SELECT ?1,source_id,current_observation,follow_file,logical_locator,association,availability FROM metadata_image_sources WHERE image_id=?2",params![target,from])?;
    db.execute("INSERT INTO metadata_assets SELECT ?1,COALESCE((SELECT revision FROM metadata_assets WHERE asset_id=?2),0)",params![target,from])?;
    db.execute("INSERT INTO metadata_choices SELECT ?1,field,model_id FROM metadata_choices WHERE asset_id=?2",params![target,from])?;
    db.execute("INSERT INTO metadata_effective SELECT ?1,field,value,conflicted,model_id FROM metadata_effective WHERE asset_id=?2",params![target,from])?;
    db.execute("INSERT INTO metadata_history(asset_id,revision,action,detail) SELECT ?1,revision,'clone',?3 FROM metadata_assets WHERE asset_id=?2",params![target,from,serde_json::json!({"source":source}).to_string()])?;
    db.execute("INSERT INTO organization_flags SELECT t.sequence,f.flag,f.provenance FROM catalog_images s JOIN organization_flags f ON f.sequence=s.sequence JOIN catalog_images t ON t.id=?1 WHERE s.id=?2", params![target,from])?;
    db.execute("INSERT INTO organization_collection_members SELECT m.collection,t.sequence,m.provenance FROM catalog_images s JOIN organization_collection_members m ON m.sequence=s.sequence JOIN catalog_images t ON t.id=?1 WHERE s.id=?2", params![target,from])?;
    db.execute("INSERT INTO organization_collection_order SELECT o.collection,t.sequence,o.position FROM catalog_images s JOIN organization_collection_order o ON o.image_sequence=s.sequence JOIN catalog_images t ON t.id=?1 WHERE s.id=?2", params![target,from])?;
    crate::organization::refresh(db, &target)?;
    Ok(())
}
impl Catalog {
    pub fn image(&self, key: &VariantKey) -> Result<Image> {
        read_image(&self.db, &id(&self.db, key)?)
    }
    pub fn browse_images(&self, after: i64, limit: usize) -> Result<Vec<Image>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "image page bounds"
        );
        self.db
            .prepare("SELECT id FROM catalog_images WHERE sequence>? ORDER BY sequence LIMIT ?")?
            .query_map(params![after, limit as i64], |r| r.get::<_, String>(0))?
            .map(|x| read_image(&self.db, &x?))
            .collect()
    }
    pub fn image_metadata_identity(&self, key: &VariantKey) -> Result<ImageMetadataIdentity> {
        let tx = self.db.unchecked_transaction()?;
        let result = identity(&tx, &id(&tx, key)?)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn with_image_metadata_identity<T>(
        &mut self,
        expected: &ImageMetadataIdentity,
        action: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let _w = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        require_image_metadata_identity(&tx, expected)?;
        let result = action(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    /// Source registrar; the caller must separately admit its immutable source manifest.
    pub fn register_import_image(&mut self, r: &ImportImageRequest) -> Result<Image> {
        let _w = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = register_import_image(&tx, r)?;
        tx.commit()?;
        Ok(result)
    }
}

pub(crate) fn enqueue_shared(db: &Connection, image: &str, observation: i64) -> Result<()> {
    let asset = physical(db, image)?;
    let source: i64 = db.query_row(
        "SELECT source_id FROM metadata_observations WHERE id=?",
        [observation],
        |r| r.get(0),
    )?;
    db.execute(
        "UPDATE image_shared_state SET epoch=epoch+1 WHERE asset_id=?",
        [&asset],
    )?;
    db.execute(
        "UPDATE assets SET physical_generation=physical_generation+1 WHERE id=?",
        [&asset],
    )?;
    db.execute("INSERT INTO image_shared_events(asset_id,epoch,source_id,observation_id,current_observation,association,availability,high_water) SELECT ?1,t.epoch,s.id,?2,s.current_observation,s.association,s.availability,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=?1) FROM metadata_sources s JOIN image_shared_state t ON t.asset_id=s.asset_id WHERE s.id=?3",params![asset,observation,source])?;
    Ok(())
}
type SharedEventRow = (
    i64,
    String,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    String,
    String,
    i64,
    i64,
);
pub(crate) fn step_refresh(db: &Connection, limit: usize) -> Result<ImageRefreshProgress> {
    ensure!((1..=1000).contains(&limit), "image refresh batch limit");
    let mut processed = 0;
    while processed < limit {
        let event:Option<SharedEventRow>=db.query_row("SELECT id,asset_id,epoch,source_id,observation_id,current_observation,association,availability,high_water,cursor FROM image_shared_events ORDER BY id LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?))).optional()?;
        let Some((
            event,
            asset,
            epoch,
            source,
            observed,
            current,
            association,
            availability,
            high,
            after,
        )) = event
        else {
            break;
        };
        let rows=db.prepare("SELECT sequence,id,applied_shared_epoch FROM catalog_images WHERE asset_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4")?.query_map(params![asset,after,high,(limit-processed) as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            db.execute("DELETE FROM image_shared_events WHERE id=?", [event])?;
            continue;
        }
        for (sequence, image, applied) in &rows {
            if *applied < epoch {
                ensure!(*applied == epoch - 1, "shared metadata event epoch gap");
                let old:Option<(Option<i64>,bool,String,String)>=db.query_row("SELECT current_observation,follow_file,association,availability FROM metadata_image_sources WHERE image_id=?1 AND source_id=?2",params![image,source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                if old.as_ref().is_none_or(|v| v.1) {
                    for oid in [observed, current].into_iter().flatten() {
                        db.execute(
                            "INSERT OR IGNORE INTO metadata_image_observations VALUES(?1,?2)",
                            params![image, oid],
                        )?;
                    }
                    db.execute("INSERT INTO metadata_image_sources SELECT ?1,id,?3,1,locator,?4,?5 FROM metadata_sources WHERE id=?2 ON CONFLICT(image_id,source_id) DO UPDATE SET current_observation=excluded.current_observation,logical_locator=excluded.logical_locator,association=excluded.association,availability=excluded.availability",params![image,source,current,association,availability])?;
                    if old
                        .as_ref()
                        .is_none_or(|v| v.0 != current || v.2 != association || v.3 != availability)
                    {
                        crate::catalog_metadata::rebuild(db, image)?;
                        crate::catalog_metadata::advance(
                            db,
                            image,
                            "shared_observation",
                            &serde_json::json!({"event":event,"observation":observed}),
                            true,
                        )?;
                    }
                }
                db.execute(
                    "UPDATE catalog_images SET applied_shared_epoch=?2 WHERE id=?1",
                    params![image, epoch],
                )?;
            }
            db.execute(
                "UPDATE image_shared_events SET cursor=?2 WHERE id=?1",
                params![event, sequence],
            )?;
        }
        processed += rows.len();
    }
    let pending = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM image_shared_events)",
        [],
        |r| r.get(0),
    )?;
    Ok(ImageRefreshProgress { processed, pending })
}
impl Catalog {
    pub fn step_image_metadata_refresh(&mut self, limit: usize) -> Result<ImageRefreshProgress> {
        let _w = self.writers.enter(Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = step_refresh(&tx, limit)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn metadata_for_image(
        &self,
        key: &VariantKey,
    ) -> Result<crate::catalog_metadata::MetadataView> {
        self.metadata(&id(&self.db, key)?)
    }
    pub fn retain_metadata_for_image(
        &mut self,
        key: &VariantKey,
        source: &crate::catalog_metadata::Source,
        inspection: &crate::xmp_packets::Inspection,
    ) -> Result<crate::catalog_metadata::Change> {
        let image = id(&self.db, key)?;
        require_current(&self.db, &image)?;
        self.retain_image_metadata_id(&image, source, inspection, false)
    }
    /// Automatic import projection is allowed only for a complete, confirmed origin.
    pub fn retain_import_metadata_for_image(
        &mut self,
        key: &VariantKey,
        source: &crate::catalog_metadata::Source,
        inspection: &crate::xmp_packets::Inspection,
    ) -> Result<crate::catalog_metadata::Change> {
        ensure!(
            inspection.status == crate::xmp_packets::Status::Complete && !source.ambiguous,
            "automatic import projection requires a Complete confirmed origin; retain incomplete evidence separately"
        );
        self.retain_metadata_for_image(key, source, inspection)
    }
    pub fn resolve_metadata_for_image(
        &mut self,
        key: &VariantKey,
        expected: i64,
        field: &str,
        model: i64,
    ) -> Result<i64> {
        let image = id(&self.db, key)?;
        self.resolve_metadata(&image, expected, field, model)
    }
    pub fn edit_metadata_for_image(
        &mut self,
        key: &VariantKey,
        expected: i64,
        base: Option<i64>,
        edits: &[crate::xmp::Edit],
    ) -> Result<crate::catalog_metadata::Change> {
        let image = id(&self.db, key)?;
        self.edit_metadata(&image, expected, base, edits)
    }
    pub fn metadata_model_for_image(&self, key: &VariantKey, model: i64) -> Result<Vec<u8>> {
        self.metadata_model(&id(&self.db, key)?, model)
    }
    pub fn metadata_history_for_image(
        &self,
        key: &VariantKey,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::catalog_metadata::Observation>> {
        self.metadata_history(&id(&self.db, key)?, after, limit)
    }
    pub fn metadata_decisions_for_image(
        &self,
        key: &VariantKey,
        after: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        self.metadata_decisions(&id(&self.db, key)?, after, limit)
    }
    pub fn metadata_packets_for_image(
        &self,
        key: &VariantKey,
        observation: i64,
    ) -> Result<Vec<crate::catalog_metadata::PacketEvidence>> {
        self.metadata_packets(&id(&self.db, key)?, observation)
    }
    pub fn organize_image(
        &mut self,
        key: &VariantKey,
        expected: i64,
        operation: crate::organization::Operation,
    ) -> Result<i64> {
        let image = id(&self.db, key)?;
        self.organize_asset(&image, expected, operation)
    }
    pub fn append_image_organization_batch(
        &mut self,
        job: &str,
        items: &[(VariantKey, i64)],
    ) -> Result<crate::organization::Job> {
        ensure!(
            !items.is_empty() && items.len() <= 1000,
            "image batch bounds"
        );
        let items = items
            .iter()
            .map(|(key, revision)| {
                Ok(crate::organization::BatchItem {
                    asset_id: id(&self.db, key)?,
                    expected_revision: *revision,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.append_organization_batch(job, &items)
    }
}

/// Reserve a not-yet-created physical asset in the importer transaction.
pub fn reserve_import_asset(db: &Connection, asset: &str, owner: &str) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "import asset reservation requires a transaction"
    );
    ensure!(
        !asset.is_empty() && !owner.is_empty() && asset.len() <= 256 && owner.len() <= 32768,
        "reservation identity bounds"
    );
    ensure!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets WHERE id=?)",
            [asset],
            |r| r.get::<_, bool>(0)
        )?,
        "existing physical asset cannot be reserved for import"
    );
    db.execute(
        "INSERT INTO image_import_reservations VALUES(?1,?2)",
        params![asset, owner],
    )?;
    Ok(())
}
/// Transaction-level image registration, for atomic importer mappings/checkpoints.
pub fn register_import_image(db: &Connection, r: &ImportImageRequest) -> Result<Image> {
    for s in [
        &r.import_source,
        &r.capture_revision,
        &r.source_table,
        &r.source_id,
        &r.input_digest,
        &r.adapter_version,
        &r.asset_id,
        &r.label,
    ] {
        ensure!(
            !s.is_empty() && s.len() <= 32768,
            "import image identity bounds"
        );
    }
    ensure!(
        r.role != ImageRole::NativeCopy,
        "import role must be source master or virtual"
    );
    ensure!(
        !db.is_autocommit(),
        "image registration requires a transaction"
    );
    let tx = db;
    if let Some((image,digest,adapter))=tx.query_row("SELECT image_id,input_digest,adapter_version FROM image_import_map WHERE import_source=?1 AND capture_revision=?2 AND source_table=?3 AND source_id=?4",params![r.import_source,r.capture_revision,r.source_table,r.source_id],|q|Ok((q.get::<_,String>(0)?,q.get::<_,String>(1)?,q.get::<_,String>(2)?))).optional()?{
            let found=read_image(tx,&image)?;ensure!(digest==r.input_digest&&adapter==r.adapter_version&&found.key.asset_id==r.asset_id&&found.role==r.role,"mapped image input/role changed");
            let parent=r.master.as_ref().map(|k|id(tx,k).and_then(|x|read_image(tx,&x).map(|i|i.sequence))).transpose()?;ensure!(parent==found.master_sequence,"mapped image parent changed");return Ok(found);
        }
    let epoch: i64 = tx.query_row(
        "SELECT epoch FROM image_shared_state WHERE asset_id=?",
        [&r.asset_id],
        |q| q.get(0),
    )?;
    let first = r.claim_reserved_master;
    if first {
        ensure!(
            r.role == ImageRole::Master,
            "only source master can claim reserved image"
        );
        ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM image_import_reservations WHERE asset_id=?1 AND owner=?2) AND NOT EXISTS(SELECT 1 FROM image_import_map WHERE image_id=?1) AND NOT EXISTS(SELECT 1 FROM edit_variants WHERE asset_id=?1)",params![r.asset_id,r.import_source],|q|q.get::<_,bool>(0))?, "reserved master not owned by import or already edited/mapped");
    }
    let parent = r
        .master
        .as_ref()
        .map(|k| {
            ensure!(k.asset_id == r.asset_id, "foreign virtual parent");
            let i = read_image(tx, &id(tx, k)?)?;
            ensure!(i.role == ImageRole::Master, "virtual parent is not master");
            Ok(i.sequence)
        })
        .transpose()?;
    ensure!(
        (r.role == ImageRole::Virtual) == parent.is_some(),
        "source parent/role mismatch"
    );
    let key = VariantKey {
        asset_id: r.asset_id.clone(),
        variant_id: if first {
            MASTER.into()
        } else {
            uuid::Uuid::new_v4().to_string()
        },
    };
    if first {
        tx.execute(
            "UPDATE catalog_images SET origin='import',translation_state='untranslated' WHERE id=?",
            [&r.asset_id],
        )?;
    } else {
        tx.execute("INSERT INTO catalog_images(id,asset_id,variant_id,role,origin,master_sequence,translation_state,applied_shared_epoch) VALUES(?1,?2,?3,?4,'import',?5,'untranslated',?6)",params![uuid::Uuid::new_v4().to_string(),r.asset_id,key.variant_id,r.role.sql(),parent,epoch])?;
    }
    let image = id(tx, &key)?;
    // Shared file evidence is linked once, never read again for a virtual image.
    tx.execute("INSERT OR IGNORE INTO metadata_image_observations SELECT ?1,o.id FROM metadata_sources s JOIN metadata_observations o ON o.id=s.current_observation WHERE s.asset_id=?2 AND s.kind IN('embedded','sidecar')",params![image,r.asset_id])?;
    tx.execute("INSERT OR IGNORE INTO metadata_image_sources SELECT ?1,s.id,s.current_observation,1,s.locator,s.association,s.availability FROM metadata_sources s WHERE s.asset_id=?2 AND s.kind IN('embedded','sidecar')",params![image,r.asset_id])?;
    if !first {
        crate::catalog_edits::insert_variant(tx, &key, &r.label, &crate::edit::Recipe::default())?;
    }
    tx.execute(
        "INSERT INTO image_import_map VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            r.import_source,
            r.capture_revision,
            r.source_table,
            r.source_id,
            r.input_digest,
            r.adapter_version,
            image
        ],
    )?;
    crate::catalog_metadata::rebuild(tx, &image)?;
    crate::organization::refresh(tx, &image)?;
    if first {
        tx.execute(
            "DELETE FROM image_import_reservations WHERE asset_id=?1 AND owner=?2",
            params![r.asset_id, r.import_source],
        )?;
    }
    let result = read_image(tx, &image)?;
    Ok(result)
}

/// Drain bounded physical-storage projection invalidations without enumerating images in triggers.
pub(crate) fn step_storage(db: &Connection, limit: usize) -> Result<usize> {
    let mut processed = 0;
    while processed < limit {
        let event:Option<(String,i64,i64)>=db.query_row("SELECT asset_id,high_water,cursor FROM image_storage_events ORDER BY asset_id LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((asset, high, after)) = event else {
            break;
        };
        let rows=db.prepare("SELECT sequence,id FROM catalog_images WHERE asset_id=?1 AND sequence>?2 AND sequence<=?3 ORDER BY sequence LIMIT ?4")?.query_map(params![asset,after,high,(limit-processed)as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.is_empty() {
            db.execute("DELETE FROM image_storage_events WHERE asset_id=?", [asset])?;
            continue;
        }
        for (seq, image) in &rows {
            crate::organization::refresh(db, image)?;
            db.execute(
                "UPDATE image_storage_events SET cursor=?2 WHERE asset_id=?1",
                params![asset, seq],
            )?;
        }
        processed += rows.len();
    }
    Ok(processed)
}
/// Storage/relink operations call this after changing a physical source's state.
pub(crate) fn enqueue_source_state(db: &Connection, source: i64) -> Result<()> {
    let asset: String = db.query_row(
        "SELECT asset_id FROM metadata_sources WHERE id=?",
        [source],
        |r| r.get(0),
    )?;
    db.execute(
        "UPDATE image_shared_state SET epoch=epoch+1 WHERE asset_id=?",
        [&asset],
    )?;
    db.execute(
        "UPDATE assets SET physical_generation=physical_generation+1 WHERE id=?",
        [&asset],
    )?;
    db.execute("INSERT INTO image_shared_events(asset_id,epoch,source_id,observation_id,current_observation,association,availability,high_water) SELECT s.asset_id,t.epoch,s.id,s.current_observation,s.current_observation,s.association,s.availability,(SELECT COALESCE(MAX(sequence),0) FROM catalog_images WHERE asset_id=s.asset_id) FROM metadata_sources s JOIN image_shared_state t ON t.asset_id=s.asset_id WHERE s.id=?",[source])?;
    Ok(())
}

#[cfg(test)]
mod tests;
