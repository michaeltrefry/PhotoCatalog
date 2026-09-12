//! Logical-image membership details layered on the existing organization dictionaries.
use super::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionPlacement {
    pub collection: String,
    pub parent: Option<String>,
    pub position: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageMembership {
    pub key: VariantKey,
    pub position: i64,
    pub provenance: serde_json::Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRelation {
    pub sequence: i64,
    pub kind: String,
    pub from: VariantKey,
    pub to: VariantKey,
    pub position: i64,
    pub provenance: serde_json::Value,
}
pub(super) const SCHEMA: &str = r#"
CREATE TABLE organization_collection_structure(collection TEXT PRIMARY KEY REFERENCES organization_collections(id) ON DELETE CASCADE,parent TEXT REFERENCES organization_collections(id),position INTEGER NOT NULL CHECK(position>=0),CHECK(parent IS NULL OR parent!=collection));
CREATE INDEX organization_collection_children ON organization_collection_structure(parent,position,collection);
CREATE TABLE organization_collection_order(collection TEXT NOT NULL,image_sequence INTEGER NOT NULL,position INTEGER NOT NULL CHECK(position>=0),PRIMARY KEY(collection,image_sequence),FOREIGN KEY(collection,image_sequence) REFERENCES organization_collection_members(collection,sequence) ON DELETE CASCADE);
CREATE INDEX organization_collection_order_page ON organization_collection_order(collection,position,image_sequence);
CREATE TABLE organization_keyword_synonyms(keyword INTEGER NOT NULL REFERENCES organization_keywords(id) ON DELETE CASCADE,synonym TEXT NOT NULL,provenance TEXT NOT NULL,PRIMARY KEY(keyword,synonym));
CREATE TABLE organization_image_relations(sequence INTEGER PRIMARY KEY AUTOINCREMENT,source_key TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,from_image TEXT NOT NULL REFERENCES catalog_images(id),to_image TEXT NOT NULL REFERENCES catalog_images(id),position INTEGER NOT NULL CHECK(position>=0),provenance TEXT NOT NULL);
CREATE INDEX organization_image_relation_from ON organization_image_relations(from_image,sequence);
"#;
fn small(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 1024 && !s.contains('\0'),
        "organization value bounds"
    );
    Ok(())
}
fn provenance(v: &serde_json::Value) -> Result<String> {
    let s = serde_json::to_string(v)?;
    ensure!(s.len() <= 65536, "organization provenance exceeds64KiB");
    Ok(s)
}
/// Transaction helper, including an ancestry check before publishing hierarchy changes.
pub fn place_collection(db: &Connection, p: &CollectionPlacement) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "collection placement requires transaction"
    );
    small(&p.collection)?;
    ensure!(p.position >= 0, "negative collection order");
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM organization_collections WHERE id=?)",
            [&p.collection],
            |r| r.get::<_, bool>(0)
        )?,
        "collection missing"
    );
    if let Some(parent) = &p.parent {
        small(parent)?;
        ensure!(parent != &p.collection, "self collection parent");
        ensure!(
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM organization_collections WHERE id=?)",
                [parent],
                |r| r.get::<_, bool>(0)
            )?,
            "parent collection missing"
        );
        // Bound ancestry independently of catalog size, rejecting excessive depth rather than looping.
        let mut cursor = Some(parent.clone());
        let mut depth = 0;
        while let Some(current) = cursor {
            ensure!(current != p.collection, "collection hierarchy cycle");
            depth += 1;
            ensure!(depth <= 256, "collection hierarchy exceeds256levels");
            cursor = db
                .query_row(
                    "SELECT parent FROM organization_collection_structure WHERE collection=?",
                    [current],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
        }
    }
    db.execute("INSERT INTO organization_collection_structure VALUES(?1,?2,?3) ON CONFLICT(collection) DO UPDATE SET parent=excluded.parent,position=excluded.position",params![p.collection,p.parent,p.position])?;
    db.execute(
        "UPDATE organization_collections SET revision=revision+1 WHERE id=?",
        [&p.collection],
    )?;
    Ok(())
}
/// Source-key upsert preserves exact logical endpoints and fails if replay changes a relation.
pub fn retain_image_relation(
    db: &Connection,
    source_key: &str,
    kind: &str,
    from: &VariantKey,
    to: &VariantKey,
    position: i64,
    evidence: &serde_json::Value,
) -> Result<i64> {
    ensure!(
        !db.is_autocommit(),
        "relationship retention requires transaction"
    );
    small(source_key)?;
    small(kind)?;
    ensure!(position >= 0, "negative relationship order");
    let a = id(db, from)?;
    let b = id(db, to)?;
    let evidence = provenance(evidence)?;
    if let Some((seq,k,f,t,p,e))=db.query_row("SELECT sequence,kind,from_image,to_image,position,provenance FROM organization_image_relations WHERE source_key=?",[source_key],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?))).optional()?{ensure!(k==kind&&f==a&&t==b&&p==position&&e==evidence,"relationship source key changed");return Ok(seq)}
    db.execute("INSERT INTO organization_image_relations(source_key,kind,from_image,to_image,position,provenance) VALUES(?1,?2,?3,?4,?5,?6)",params![source_key,kind,a,b,position,evidence])?;
    Ok(db.last_insert_rowid())
}
impl Catalog {
    pub fn place_collection(&mut self, placement: &CollectionPlacement) -> Result<()> {
        let _w = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        place_collection(&tx, placement)?;
        tx.commit()?;
        Ok(())
    }
    pub fn collection_placement(&self, collection: &str) -> Result<CollectionPlacement> {
        Ok(self.db.query_row("SELECT c.id,s.parent,COALESCE(s.position,0) FROM organization_collections c LEFT JOIN organization_collection_structure s ON s.collection=c.id WHERE c.id=?",[collection],|r|Ok(CollectionPlacement{collection:r.get(0)?,parent:r.get(1)?,position:r.get(2)?}))?)
    }
    pub fn set_image_collection_membership(
        &mut self,
        expected: &ImageMetadataIdentity,
        collection: &str,
        position: i64,
        evidence: &serde_json::Value,
    ) -> Result<i64> {
        small(collection)?;
        ensure!(position >= 0, "negative membership order");
        let evidence = provenance(evidence)?;
        let _w = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        require_image_metadata_identity(&tx, expected)?;
        let seq: i64 = tx.query_row(
            "SELECT sequence FROM catalog_images WHERE id=?",
            [&expected.image_id],
            |r| r.get(0),
        )?;
        tx.execute("INSERT INTO organization_collection_members VALUES(?1,?2,?3) ON CONFLICT(collection,sequence) DO UPDATE SET provenance=excluded.provenance",params![collection,seq,evidence])?;
        tx.execute("INSERT INTO organization_collection_order VALUES(?1,?2,?3) ON CONFLICT(collection,image_sequence) DO UPDATE SET position=excluded.position",params![collection,seq,position])?;
        let revision = crate::catalog_metadata::advance(
            &tx,
            &expected.image_id,
            "collection_membership",
            &serde_json::json!({"collection":collection,"position":position}),
            false,
        )?;
        tx.execute(
            "UPDATE organization_collections SET revision=revision+1 WHERE id=?",
            [collection],
        )?;
        tx.commit()?;
        Ok(revision)
    }
    pub fn image_collection_members(
        &self,
        collection: &str,
        after: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<(i64, ImageMembership)>> {
        ensure!((1..=1000).contains(&limit), "membership page bounds");
        let (position, sequence) = after.unwrap_or((-1, 0));
        ensure!(position >= -1 && sequence >= 0, "membership cursor bounds");
        self.db.prepare("SELECT i.sequence,i.asset_id,i.variant_id,COALESCE(o.position,0),m.provenance FROM organization_collection_members m JOIN catalog_images i ON i.sequence=m.sequence LEFT JOIN organization_collection_order o ON o.collection=m.collection AND o.image_sequence=m.sequence WHERE m.collection=?1 AND (COALESCE(o.position,0),i.sequence)>(?2,?3) ORDER BY COALESCE(o.position,0),i.sequence LIMIT ?4")?.query_map(params![collection,position,sequence,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.map(|r|{let(seq,a,v,p,e)=r?;Ok((seq,ImageMembership{key:VariantKey{asset_id:a,variant_id:v},position:p,provenance:serde_json::from_str(&e)?}))}).collect()
    }
    pub fn add_keyword_synonym(
        &mut self,
        keyword: i64,
        synonym: &str,
        evidence: &serde_json::Value,
    ) -> Result<()> {
        small(synonym)?;
        let evidence = provenance(evidence)?;
        let _w = self.writers.enter(Priority::Foreground)?;
        self.db.execute("INSERT INTO organization_keyword_synonyms VALUES(?1,?2,?3) ON CONFLICT(keyword,synonym) DO UPDATE SET provenance=excluded.provenance",params![keyword,synonym,evidence])?;
        Ok(())
    }
    pub fn keyword_synonyms(
        &self,
        keyword: i64,
        after: &str,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        ensure!((1..=1000).contains(&limit), "synonym page bounds");
        self.db.prepare("SELECT synonym,provenance FROM organization_keyword_synonyms WHERE keyword=?1 AND synonym>?2 ORDER BY synonym LIMIT ?3")?.query_map(params![keyword,after,limit as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.map(|r|{let(s,p)=r?;Ok((s,serde_json::from_str(&p)?))}).collect()
    }
    pub fn retain_image_relation(
        &mut self,
        source_key: &str,
        kind: &str,
        from: &VariantKey,
        to: &VariantKey,
        position: i64,
        evidence: &serde_json::Value,
    ) -> Result<i64> {
        let _w = self.writers.enter(Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = retain_image_relation(&tx, source_key, kind, from, to, position, evidence)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn image_relations(
        &self,
        from: &VariantKey,
        after: i64,
        limit: usize,
    ) -> Result<Vec<ImageRelation>> {
        ensure!(
            after >= 0 && (1..=1000).contains(&limit),
            "relationship page bounds"
        );
        let image = id(&self.db, from)?;
        self.db.prepare("SELECT r.sequence,r.kind,t.asset_id,t.variant_id,r.position,r.provenance FROM organization_image_relations r JOIN catalog_images t ON t.id=r.to_image WHERE r.from_image=?1 AND r.sequence>?2 ORDER BY r.sequence LIMIT ?3")?.query_map(params![image,after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?)))?.map(|r|{let(seq,k,a,v,p,e)=r?;Ok(ImageRelation{sequence:seq,kind:k,from:from.clone(),to:VariantKey{asset_id:a,variant_id:v},position:p,provenance:serde_json::from_str(&e)?})}).collect()
    }
}
