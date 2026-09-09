//! Durable organization projections. Source packets and decisions remain in S4.
use crate::{
    Catalog, Metadata,
    storage_volume::NativePath,
    xmp::{self, Edit, Value},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE organization_state(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL DEFAULT 0, backfill_after INTEGER NOT NULL DEFAULT 0, backfill_high INTEGER NOT NULL);
INSERT INTO organization_state(id,backfill_high) SELECT 1,COALESCE(MAX(sequence),0) FROM assets;
CREATE TABLE organization_dirty(sequence INTEGER PRIMARY KEY REFERENCES assets(sequence));
CREATE TABLE organization_folders(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES organization_folders(id), locator TEXT NOT NULL UNIQUE, name TEXT NOT NULL);
CREATE INDEX organization_folder_parent ON organization_folders(parent,id);
CREATE TABLE organization_folder_members(folder INTEGER NOT NULL REFERENCES organization_folders(id), sequence INTEGER NOT NULL REFERENCES assets(sequence), direct INTEGER NOT NULL, PRIMARY KEY(folder,sequence));
CREATE INDEX organization_folder_asset ON organization_folder_members(sequence,folder);
CREATE TABLE organization_keywords(id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN('flat','hierarchical')), parent INTEGER REFERENCES organization_keywords(id), name TEXT NOT NULL, path TEXT NOT NULL, UNIQUE(kind,path));
CREATE INDEX organization_keyword_parent ON organization_keywords(kind,parent,id);
CREATE TABLE organization_keyword_members(keyword INTEGER NOT NULL REFERENCES organization_keywords(id), sequence INTEGER NOT NULL REFERENCES assets(sequence), direct INTEGER NOT NULL, model_id INTEGER REFERENCES metadata_models(id), PRIMARY KEY(keyword,sequence));
CREATE INDEX organization_keyword_asset ON organization_keyword_members(sequence,keyword);
CREATE TABLE organization_collections(id TEXT PRIMARY KEY,name TEXT NOT NULL,provenance TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE organization_collection_members(collection TEXT NOT NULL REFERENCES organization_collections(id),sequence INTEGER NOT NULL REFERENCES assets(sequence),provenance TEXT NOT NULL,PRIMARY KEY(collection,sequence));
CREATE INDEX organization_collection_asset ON organization_collection_members(sequence,collection);
CREATE TABLE organization_flags(sequence INTEGER PRIMARY KEY REFERENCES assets(sequence),flag TEXT NOT NULL CHECK(flag IN('unflagged','pick','reject')),provenance TEXT NOT NULL);
CREATE TABLE organization_assets(sequence INTEGER PRIMARY KEY REFERENCES assets(sequence),asset_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL, metadata_revision INTEGER NOT NULL, folder INTEGER REFERENCES organization_folders(id),filename TEXT NOT NULL,capture TEXT NOT NULL,camera_make TEXT NOT NULL,camera TEXT NOT NULL,lens TEXT NOT NULL,format TEXT NOT NULL,rating INTEGER NOT NULL,flag TEXT NOT NULL,label TEXT NOT NULL,conflicts TEXT NOT NULL,provenance TEXT NOT NULL,search_text TEXT NOT NULL);
CREATE INDEX organization_sort_capture ON organization_assets(capture,sequence);
CREATE INDEX organization_sort_filename ON organization_assets(filename,sequence);
CREATE INDEX organization_sort_rating ON organization_assets(rating,sequence);
CREATE INDEX organization_label_sequence ON organization_assets(label,sequence);
CREATE INDEX organization_flag_sequence ON organization_assets(flag,sequence);
CREATE INDEX organization_format_sequence ON organization_assets(format,sequence);
CREATE INDEX organization_camera_sequence ON organization_assets(camera,sequence);
CREATE INDEX organization_lens_sequence ON organization_assets(lens,sequence);
CREATE INDEX organization_folder_sequence ON organization_assets(folder,sequence);
CREATE INDEX organization_folder_capture ON organization_assets(folder,capture,sequence);
CREATE INDEX organization_rating_capture ON organization_assets(rating,capture,sequence);
CREATE VIRTUAL TABLE organization_text USING fts5(text, tokenize='unicode61');
CREATE TABLE organization_jobs(id TEXT PRIMARY KEY,operation TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('preparing','ready','running','paused','complete','cancelled')),total INTEGER NOT NULL DEFAULT 0,applied INTEGER NOT NULL DEFAULT 0,failed INTEGER NOT NULL DEFAULT 0,skipped INTEGER NOT NULL DEFAULT 0,created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')));
CREATE TABLE organization_job_items(job TEXT NOT NULL REFERENCES organization_jobs(id),sequence INTEGER NOT NULL REFERENCES assets(sequence),expected_revision INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN('pending','applied','failed','skipped')),error TEXT,result_revision INTEGER,PRIMARY KEY(job,sequence));
CREATE INDEX organization_job_pending ON organization_job_items(job,status,sequence);
CREATE TABLE organization_events(id INTEGER PRIMARY KEY,job TEXT REFERENCES organization_jobs(id),sequence INTEGER REFERENCES assets(sequence),action TEXT NOT NULL,detail TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%fZ','now')));
CREATE INDEX organization_events_job ON organization_events(job,id);
CREATE TRIGGER organization_asset_insert AFTER INSERT ON assets BEGIN INSERT INTO organization_dirty SELECT new.sequence WHERE NOT EXISTS(SELECT 1 FROM organization_dirty WHERE sequence=new.sequence); END;
CREATE TRIGGER organization_asset_update AFTER UPDATE OF metadata,path_display,state,location ON assets BEGIN INSERT INTO organization_dirty SELECT new.sequence WHERE NOT EXISTS(SELECT 1 FROM organization_dirty WHERE sequence=new.sequence); END;
CREATE TRIGGER organization_metadata_update AFTER UPDATE ON metadata_assets BEGIN INSERT INTO organization_dirty SELECT sequence FROM assets WHERE id=new.asset_id AND NOT EXISTS(SELECT 1 FROM organization_dirty WHERE sequence=assets.sequence); END;
CREATE TRIGGER organization_binding_insert AFTER INSERT ON storage_bindings BEGIN INSERT INTO organization_dirty SELECT sequence FROM assets WHERE id=new.asset_id AND NOT EXISTS(SELECT 1 FROM organization_dirty WHERE sequence=assets.sequence); END;
CREATE TRIGGER organization_binding_update AFTER UPDATE ON storage_bindings BEGIN INSERT INTO organization_dirty SELECT sequence FROM assets WHERE id=new.asset_id AND NOT EXISTS(SELECT 1 FROM organization_dirty WHERE sequence=assets.sequence); END;
"#;

pub(crate) const CAPTURE_LENS_SCHEMA: &str =
    "CREATE INDEX organization_lens_capture ON organization_assets(lens,capture,sequence);";

pub const MAX_BATCH: usize = 1000;
const LR: &str = "http://ns.adobe.com/lightroom/1.0/";
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeywordKind {
    Flat,
    Hierarchical,
}
impl KeywordKind {
    fn sql(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Hierarchical => "hierarchical",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Unflagged,
    Pick,
    Reject,
}
impl Flag {
    pub(crate) fn sql(&self) -> &'static str {
        match self {
            Self::Unflagged => "unflagged",
            Self::Pick => "pick",
            Self::Reject => "reject",
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Keyword {
    pub id: i64,
    pub kind: KeywordKind,
    pub parent: Option<i64>,
    pub name: String,
    pub path: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Folder {
    pub id: i64,
    pub parent: Option<i64>,
    pub locator: NativePath,
    pub name: String,
}
#[derive(Debug, Serialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub revision: i64,
    pub provenance: serde_json::Value,
}
#[derive(Debug, Serialize)]
pub struct IndexProgress {
    pub processed: usize,
    pub backfill_after: i64,
    pub backfill_high: i64,
    pub pending: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Rating {
        value: u8,
    },
    Label {
        value: String,
    },
    Flag {
        value: Flag,
    },
    AddKeyword {
        kind: KeywordKind,
        path: Vec<String>,
    },
    RemoveKeyword {
        kind: KeywordKind,
        path: Vec<String>,
    },
    MoveKeyword {
        from: Vec<String>,
        to: Vec<String>,
    },
    AddCollection {
        collection: String,
    },
    RemoveCollection {
        collection: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItem {
    pub asset_id: String,
    pub expected_revision: i64,
}
#[derive(Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub operation: Operation,
    pub state: String,
    pub pending: i64,
    pub applied: i64,
    pub failed: i64,
    pub skipped: i64,
}
#[derive(Debug, Serialize)]
pub struct JobItem {
    pub sequence: i64,
    pub asset_id: String,
    pub expected_revision: i64,
    pub status: String,
    pub error: Option<String>,
    pub result_revision: Option<i64>,
}
fn text_limit(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1024 && !value.contains('\0'),
        "name must contain 1..1024 bytes and no NUL"
    );
    Ok(())
}
fn path_valid(kind: KeywordKind, path: &[String]) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 64,
        "keyword hierarchy depth must be 1..64"
    );
    ensure!(
        kind != KeywordKind::Flat || path.len() == 1,
        "flat keywords have one component"
    );
    for v in path {
        text_limit(v)?;
        ensure!(
            kind == KeywordKind::Flat || !v.contains('|'),
            "hierarchical keyword components cannot contain the XMP separator '|'"
        );
    }
    Ok(())
}
fn keyword(db: &Connection, kind: KeywordKind, path: &[String]) -> Result<i64> {
    path_valid(kind, path)?;
    let mut parent = None;
    for depth in 1..=path.len() {
        let key = serde_json::to_string(&path[..depth])?;
        db.execute("INSERT OR IGNORE INTO organization_keywords(kind,parent,name,path) VALUES(?1,?2,?3,?4)",params![kind.sql(),parent,path[depth-1],key])?;
        parent = Some(db.query_row(
            "SELECT id FROM organization_keywords WHERE kind=?1 AND path=?2",
            params![kind.sql(), key],
            |r| r.get::<_, i64>(0),
        )?);
    }
    parent.context("empty keyword")
}
/// A photographic calendar value, without inventing a timezone for EXIF dates.
/// Exact source spelling remains in retained metadata; comparisons use local date/time.
pub(crate) fn date_key(raw: &str) -> Option<String> {
    let b = raw.as_bytes();
    if b.len() < 10
        || !b[..4].iter().all(u8::is_ascii_digit)
        || !matches!(b[4], b'-' | b':')
        || b[7] != b[4]
    {
        return None;
    }
    let n = |a: usize, z: usize| std::str::from_utf8(&b[a..z]).ok()?.parse::<u32>().ok();
    let year = n(0, 4)?;
    let month = n(5, 7)?;
    let day = n(8, 10)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > max {
        return None;
    }
    let (h, m, s) = if b.len() == 10 {
        (0, 0, 0)
    } else {
        if b.len() < 19 || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
            return None;
        };
        let (h, m, s) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
        if h > 23 || m > 59 || s > 60 {
            return None;
        };
        (h, m, s)
    };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}"
    ))
}
fn folders(db: &Connection, seq: i64, asset: &str) -> Result<(Option<i64>, Option<String>)> {
    db.execute(
        "DELETE FROM organization_folder_members WHERE sequence=?",
        [seq],
    )?;
    let path: Option<String> = db
        .query_row(
            "SELECT native_path FROM storage_bindings WHERE asset_id=?",
            [asset],
            |r| r.get(0),
        )
        .optional()?;
    let Some(path) = path else {
        return Ok((None, None));
    };
    let path: NativePath = serde_json::from_str(&path)?;
    let filename = match &path {
        NativePath::UnixBytes(v) => {
            String::from_utf8_lossy(v.rsplit(|b| *b == b'/').next().unwrap_or(v)).into_owned()
        }
        NativePath::WindowsWide(v) => {
            String::from_utf16_lossy(v.rsplit(|b| *b == 47 || *b == 92).next().unwrap_or(v))
        }
    };
    let parts = crate::catalog_storage::native_folder_chain(&path)?;
    let mut parent = None;
    for (i, (native, name)) in parts.iter().enumerate() {
        let key = serde_json::to_string(native)?;
        db.execute(
            "INSERT OR IGNORE INTO organization_folders(parent,locator,name) VALUES(?1,?2,?3)",
            params![parent, key, name],
        )?;
        let id: i64 = db.query_row(
            "SELECT id FROM organization_folders WHERE locator=?",
            [key],
            |r| r.get(0),
        )?;
        db.execute(
            "INSERT INTO organization_folder_members VALUES(?1,?2,?3)",
            params![id, seq, i + 1 == parts.len()],
        )?;
        parent = Some(id);
    }
    Ok((parent, Some(filename)))
}
/// Refresh only one asset, inside the caller's transaction; never reads image files.
pub(crate) fn refresh(db: &Connection, asset: &str) -> Result<()> {
    let (seq, display, state, metadata): (i64, String, String, Option<String>) = db.query_row(
        "SELECT sequence,path_display,state,metadata FROM assets WHERE id=?",
        [asset],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let metadata = metadata
        .map(|v| serde_json::from_str::<Metadata>(&v))
        .transpose()?;
    let revision: i64 = db.query_row(
        "SELECT COALESCE((SELECT revision FROM metadata_assets WHERE asset_id=?),0)",
        [asset],
        |r| r.get(0),
    )?;
    let fields = db
        .prepare("SELECT field,value,conflicted,model_id FROM metadata_effective WHERE asset_id=?")?
        .query_map([asset], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut values = BTreeMap::new();
    let mut conflicts = BTreeSet::new();
    let mut provenance = BTreeMap::new();
    for (field, value, conflict, model) in fields {
        if conflict {
            conflicts.insert(field.clone());
        }
        if let Some(v) = value {
            values.insert(field.clone(), serde_json::from_str::<Value>(&v)?);
        }
        provenance.insert(field, model);
    }
    let scalar = |name: &str| match values.get(name) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let fallback = |name: &str, value: Option<String>| {
        if provenance.contains_key(name) || conflicts.contains(name) {
            scalar(name)
        } else {
            value
        }
    };
    let capture = fallback(
        "capture_date",
        metadata.as_ref().and_then(|v| v.captured_at.clone()),
    )
    .and_then(|v| date_key(&v))
    .unwrap_or_default();
    let camera_make = fallback(
        "camera_make",
        metadata.as_ref().and_then(|v| v.camera_make.clone()),
    )
    .unwrap_or_default();
    let camera = fallback(
        "camera_model",
        metadata.as_ref().and_then(|v| v.camera_model.clone()),
    )
    .unwrap_or_default();
    let lens = fallback("lens", metadata.as_ref().and_then(|v| v.lens.clone())).unwrap_or_default();
    let format = metadata
        .as_ref()
        .map(|v| v.format.to_ascii_uppercase())
        .unwrap_or_default();
    let rating = scalar("rating")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| (-1..=5).contains(v))
        .unwrap_or(-2);
    let label = scalar("label").unwrap_or_default();
    let flag_override: Option<(String, String)> = db
        .query_row(
            "SELECT flag,provenance FROM organization_flags WHERE sequence=?",
            [seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let flag = flag_override
        .as_ref()
        .map(|v| v.0.clone())
        .unwrap_or_else(|| {
            if rating == -1 {
                "reject".into()
            } else {
                "unflagged".into()
            }
        });
    let (folder, filename) = folders(db, seq, asset)?;
    db.execute(
        "DELETE FROM organization_keyword_members WHERE sequence=?",
        [seq],
    )?;
    for (field, kind) in [
        ("keywords", KeywordKind::Flat),
        ("hierarchical_keywords", KeywordKind::Hierarchical),
    ] {
        if let Some(Value::List(terms)) = values.get(field) {
            for term in terms {
                let path: Vec<String> = if kind == KeywordKind::Flat {
                    vec![term.clone()]
                } else {
                    term.split('|').map(str::to_owned).collect()
                };
                if path_valid(kind, &path).is_err() {
                    conflicts.insert(format!("{field}:invalid_hierarchy"));
                    continue;
                }
                keyword(db, kind, &path)?;
                for depth in 1..=path.len() {
                    let id: i64 = db.query_row(
                        "SELECT id FROM organization_keywords WHERE kind=?1 AND path=?2",
                        params![kind.sql(), serde_json::to_string(&path[..depth])?],
                        |r| r.get(0),
                    )?;
                    db.execute("INSERT INTO organization_keyword_members VALUES(?1,?2,?3,?4) ON CONFLICT(keyword,sequence) DO UPDATE SET direct=MAX(direct,excluded.direct)",params![id,seq,depth==path.len(),provenance.get(field).copied().flatten()])?;
                }
            }
        }
    }
    let filename =
        filename.unwrap_or_else(|| display.rsplit('/').next().unwrap_or(&display).to_string());
    let mut terms = vec![
        filename.clone(),
        camera_make.clone(),
        camera.clone(),
        lens.clone(),
    ];
    for field in [
        "title",
        "description",
        "creator",
        "keywords",
        "hierarchical_keywords",
    ] {
        match values.get(field) {
            Some(Value::Text(s)) => terms.push(s.clone()),
            Some(Value::List(v)) => terms.extend(v.clone()),
            Some(Value::Localized(v)) => terms.extend(v.values().cloned()),
            _ => {}
        }
    }
    let search_text = terms.join(" ");
    let conflicts = serde_json::to_string(&conflicts)?;
    let provenance = serde_json::json!({"metadata_models":provenance,"flag": flag_override.map(|v| serde_json::from_str::<serde_json::Value>(&v.1)).transpose()?}).to_string();
    let existed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM organization_assets WHERE sequence=?)",
        [seq],
        |r| r.get(0),
    )?;
    db.execute("INSERT INTO organization_assets VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) ON CONFLICT(sequence) DO UPDATE SET state=excluded.state,metadata_revision=excluded.metadata_revision,folder=excluded.folder,filename=excluded.filename,capture=excluded.capture,camera_make=excluded.camera_make,camera=excluded.camera,lens=excluded.lens,format=excluded.format,rating=excluded.rating,flag=excluded.flag,label=excluded.label,conflicts=excluded.conflicts,provenance=excluded.provenance,search_text=excluded.search_text",params![seq,asset,state,revision,folder,filename,capture,camera_make,camera,lens,format,rating,flag,label,conflicts,provenance,search_text])?;
    db.execute("DELETE FROM organization_text WHERE rowid=?", [seq])?;
    db.execute(
        "INSERT INTO organization_text(rowid,text) VALUES(?1,?2)",
        params![seq, search_text],
    )?;
    if existed {
        db.execute("UPDATE organization_state SET epoch=epoch+1 WHERE id=1", [])?;
    }
    db.execute("DELETE FROM organization_dirty WHERE sequence=?", [seq])?;
    Ok(())
}

impl Catalog {
    pub fn organization_index(&mut self, limit: usize) -> Result<IndexProgress> {
        ensure!(
            (1..=MAX_BATCH).contains(&limit),
            "index batch limit must be 1..1000"
        );
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Background)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (mut after, high): (i64, i64) = tx.query_row(
            "SELECT backfill_after,backfill_high FROM organization_state WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let rows: Vec<(i64, String)> = if after < high {
            tx.prepare("SELECT sequence,id FROM assets WHERE sequence>?1 AND sequence<=?2 ORDER BY sequence LIMIT ?3")?.query_map(params![after,high,limit as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        } else {
            tx.prepare("SELECT d.sequence,a.id FROM organization_dirty d JOIN assets a ON a.sequence=d.sequence ORDER BY d.sequence LIMIT ?1")?.query_map([limit as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (_, asset) in &rows {
            refresh(&tx, asset)?;
        }
        if after < high {
            after = rows.last().map(|v| v.0).unwrap_or(high);
            tx.execute(
                "UPDATE organization_state SET backfill_after=? WHERE id=1",
                [after],
            )?;
        }
        let pending = after < high
            || tx.query_row("SELECT EXISTS(SELECT 1 FROM organization_dirty)", [], |r| {
                r.get::<_, bool>(0)
            })?;
        tx.commit()?;
        drop(_write);
        Ok(IndexProgress {
            processed: rows.len(),
            backfill_after: after,
            backfill_high: high,
            pending,
        })
    }
    pub fn organization_folders(
        &self,
        parent: Option<i64>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Folder>> {
        page_limit(limit)?;
        let mut result = Vec::new();
        for r in self.db.prepare("SELECT id,parent,locator,name FROM organization_folders WHERE parent IS ?1 AND id>?2 ORDER BY id LIMIT ?3")?.query_map(params![parent,after,limit as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get::<_,String>(2)?,r.get(3)?)))?{let(id,parent,locator,name)=r?;result.push(Folder{id,parent,locator:serde_json::from_str(&locator)?,name});}
        Ok(result)
    }
    pub fn create_keyword(&mut self, kind: KeywordKind, path: &[String]) -> Result<i64> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self.db.transaction()?;
        let id = keyword(&tx, kind, path)?;
        tx.commit()?;
        drop(_write);
        Ok(id)
    }
    pub fn organization_keywords(
        &self,
        kind: KeywordKind,
        parent: Option<i64>,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Keyword>> {
        page_limit(limit)?;
        let mut result = Vec::new();
        for r in self.db.prepare("SELECT id,parent,name,path FROM organization_keywords WHERE kind=?1 AND parent IS ?2 AND id>?3 ORDER BY id LIMIT ?4")?.query_map(params![kind.sql(),parent,after,limit as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get::<_,String>(3)?)))?{let(id,parent,name,path)=r?;result.push(Keyword{id,kind,parent,name,path:serde_json::from_str(&path)?});}
        Ok(result)
    }
    /// Removing an in-use hierarchy requires an explicit resumable remove/move batch first.
    pub fn delete_keyword(&mut self, id: i64) -> Result<()> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM organization_keyword_members WHERE keyword=?1) OR EXISTS(SELECT 1 FROM organization_keywords WHERE kind='hierarchical' AND parent=?1)",[id],|r|r.get::<_,bool>(0))?,"keyword has members or children; remove assignments/children explicitly first");
        ensure!(
            tx.execute("DELETE FROM organization_keywords WHERE id=?", [id])? == 1,
            "keyword not found"
        );
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    pub fn create_collection(
        &mut self,
        name: &str,
        provenance: serde_json::Value,
    ) -> Result<String> {
        text_limit(name)?;
        ensure!(
            serde_json::to_vec(&provenance)?.len() <= 65536,
            "collection provenance exceeds 64KiB"
        );
        let id = uuid::Uuid::new_v4().to_string();
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        self.db.execute(
            "INSERT INTO organization_collections(id,name,provenance) VALUES(?1,?2,?3)",
            params![id, name, serde_json::to_string(&provenance)?],
        )?;
        Ok(id)
    }
    pub fn organization_collections(&self, after: &str, limit: usize) -> Result<Vec<Collection>> {
        page_limit(limit)?;
        let mut result = Vec::new();
        for r in self.db.prepare("SELECT id,name,revision,provenance FROM organization_collections WHERE id>?1 ORDER BY id LIMIT ?2")?.query_map(params![after,limit as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get::<_,String>(3)?)))?{let(id,name,revision,provenance)=r?;result.push(Collection{id,name,revision,provenance:serde_json::from_str(&provenance)?});}
        Ok(result)
    }
    pub fn rename_collection(&mut self, id: &str, revision: i64, name: &str) -> Result<()> {
        text_limit(name)?;
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        ensure!(self.db.execute("UPDATE organization_collections SET name=?3,revision=revision+1 WHERE id=?1 AND revision=?2",params![id,revision,name])?==1,"collection changed or missing");
        Ok(())
    }
    pub fn delete_collection(&mut self, id: &str, revision: i64) -> Result<()> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM organization_collection_members WHERE collection=?)",
                [id],
                |r| r.get::<_, bool>(0)
            )?,
            "collection has members; use an explicit removal batch first"
        );
        ensure!(
            tx.execute(
                "DELETE FROM organization_collections WHERE id=?1 AND revision=?2",
                params![id, revision]
            )? == 1,
            "collection changed or missing"
        );
        tx.commit()?;
        drop(_write);
        Ok(())
    }
    pub fn begin_organization_batch(&mut self, operation: Operation) -> Result<Job> {
        validate_operation(&self.db, &operation)?;
        let id = uuid::Uuid::new_v4().to_string();
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        self.db.execute(
            "INSERT INTO organization_jobs(id,operation,state) VALUES(?1,?2,'preparing')",
            params![id, serde_json::to_string(&operation)?],
        )?;
        drop(_write);
        self.organization_job(&id)
    }
    /// Persist exact selection pages before sealing. Duplicate IDs with different
    /// revision claims are rejected instead of silently updating the review plan.
    pub fn append_organization_batch(&mut self, job: &str, items: &[BatchItem]) -> Result<Job> {
        ensure!(
            !items.is_empty() && items.len() <= MAX_BATCH,
            "append batch must contain 1..1000 assets"
        );
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String = tx.query_row(
            "SELECT state FROM organization_jobs WHERE id=?",
            [job],
            |r| r.get(0),
        )?;
        ensure!(state == "preparing", "batch selection is sealed");
        let mut added = 0;
        for item in items {
            let seq: i64 = tx.query_row(
                "SELECT sequence FROM assets WHERE id=?",
                [&item.asset_id],
                |r| r.get(0),
            )?;
            let previous:Option<i64>=tx.query_row("SELECT expected_revision FROM organization_job_items WHERE job=?1 AND sequence=?2",params![job,seq],|r|r.get(0)).optional()?;
            ensure!(
                previous.is_none_or(|v| v == item.expected_revision),
                "duplicate batch item has a different revision"
            );
            added+=tx.execute("INSERT OR IGNORE INTO organization_job_items(job,sequence,expected_revision) VALUES(?1,?2,?3)",params![job,seq,item.expected_revision])?;
        }
        tx.execute(
            "UPDATE organization_jobs SET total=total+?2 WHERE id=?1",
            params![job, added as i64],
        )?;
        tx.commit()?;
        drop(_write);
        self.organization_job(job)
    }
    pub fn seal_organization_batch(&mut self, job: &str) -> Result<Job> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        ensure!(self.db.execute("UPDATE organization_jobs SET state=CASE WHEN total=0 THEN 'complete' ELSE 'ready' END WHERE id=? AND state='preparing'",[job])?==1,"batch is not preparing");
        drop(_write);
        self.organization_job(job)
    }
    pub fn organization_job(&self, job: &str) -> Result<Job> {
        let (operation, state, total, applied, failed, skipped): (
            String,
            String,
            i64,
            i64,
            i64,
            i64,
        ) = self.db.query_row(
            "SELECT operation,state,total,applied,failed,skipped FROM organization_jobs WHERE id=?",
            [job],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        Ok(Job {
            id: job.into(),
            operation: serde_json::from_str(&operation)?,
            state,
            pending: total - applied - failed - skipped,
            applied,
            failed,
            skipped,
        })
    }
    pub fn organization_jobs(&self, after: &str, limit: usize) -> Result<Vec<Job>> {
        page_limit(limit)?;
        let ids: Vec<String> = self
            .db
            .prepare("SELECT id FROM organization_jobs WHERE id>?1 ORDER BY id LIMIT ?2")?
            .query_map(params![after, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.iter().map(|id| self.organization_job(id)).collect()
    }
    pub fn organization_job_items(
        &self,
        job: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<JobItem>> {
        page_limit(limit)?;
        Ok(self.db.prepare("SELECT j.sequence,a.id,j.expected_revision,j.status,j.error,j.result_revision FROM organization_job_items j JOIN assets a ON a.sequence=j.sequence WHERE job=?1 AND j.sequence>?2 ORDER BY j.sequence LIMIT ?3")?.query_map(params![job,after,limit as i64],|r|Ok(JobItem{sequence:r.get(0)?,asset_id:r.get(1)?,expected_revision:r.get(2)?,status:r.get(3)?,error:r.get(4)?,result_revision:r.get(5)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn cancel_organization_batch(&mut self, job: &str) -> Result<Job> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        self.db.execute(
            "UPDATE organization_jobs SET state='cancelled' WHERE id=? AND state!='complete'",
            [job],
        )?;
        drop(_write);
        self.organization_job(job)
    }
    /// One asset and its progress record commit together. A failed asset is paused
    /// and visible; already acknowledged assets are never replayed on restart.
    pub fn step_organization_batch(&mut self, job: &str) -> Result<Job> {
        self.step_organization_batch_with(job, || Ok(()))
    }
    #[doc(hidden)]
    pub fn step_organization_batch_with(
        &mut self,
        job: &str,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<Job> {
        let current = self.organization_job(job)?;
        ensure!(
            matches!(current.state.as_str(), "ready" | "running"),
            "batch must be ready or running"
        );
        let row:Option<(i64,String,i64)>=self.db.query_row("SELECT j.sequence,a.id,j.expected_revision FROM organization_job_items j JOIN assets a ON a.sequence=j.sequence WHERE job=?1 AND j.status='pending' ORDER BY j.sequence LIMIT 1",[job],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((seq, asset, revision)) = row else {
            return self.organization_job(job);
        };
        let result = (|| -> Result<()> {
            ensure!(
                self.render_identity(&asset)?.metadata_revision == revision,
                "asset changed after batch review"
            );
            validate_operation(&self.db, &current.operation)?;
            match &current.operation {
                Operation::Flag { value } => {
                    let _write = self
                        .writers
                        .enter(crate::catalog_writer::Priority::Foreground)?;
                    let tx = self
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    assert_pending(&tx, job, seq, revision)?;
                    tx.execute("INSERT INTO organization_flags VALUES(?1,?2,?3) ON CONFLICT(sequence) DO UPDATE SET flag=excluded.flag,provenance=excluded.provenance",params![seq,value.sql(),serde_json::json!({"job":job,"operation":current.operation}).to_string()])?;
                    let next = advance_local(&tx, &asset, job)?;
                    refresh(&tx, &asset)?;
                    complete_item(&tx, job, seq, next)?;
                    before_commit()?;
                    tx.commit()?;
                    drop(_write);
                }
                Operation::AddCollection { collection }
                | Operation::RemoveCollection { collection } => {
                    let _write = self
                        .writers
                        .enter(crate::catalog_writer::Priority::Foreground)?;
                    let tx = self
                        .db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    assert_pending(&tx, job, seq, revision)?;
                    validate_operation(&tx, &current.operation)?;
                    if matches!(current.operation, Operation::AddCollection { .. }) {
                        tx.execute("INSERT INTO organization_collection_members VALUES(?1,?2,?3) ON CONFLICT(collection,sequence) DO NOTHING",params![collection,seq,serde_json::json!({"job":job}).to_string()])?;
                    } else {
                        tx.execute("DELETE FROM organization_collection_members WHERE collection=?1 AND sequence=?2",params![collection,seq])?;
                    }
                    tx.execute(
                        "UPDATE organization_collections SET revision=revision+1 WHERE id=?",
                        [collection],
                    )?;
                    let next = advance_local(&tx, &asset, job)?;
                    refresh(&tx, &asset)?;
                    complete_item(&tx, job, seq, next)?;
                    before_commit()?;
                    tx.commit()?;
                    drop(_write);
                }
                _ => {
                    let (base, fields, edits) =
                        organization_edits(self, &asset, &current.operation)?;
                    self.edit_metadata_commit(
                        &asset,
                        revision,
                        base,
                        &edits,
                        &fields,
                        |db, next| {
                            assert_pending(db, job, seq, next)?;
                            complete_item(db, job, seq, next)?;
                            before_commit()
                        },
                    )?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _write = self
                .writers
                .enter(crate::catalog_writer::Priority::Foreground)?;
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let changed=tx.execute("UPDATE organization_job_items SET status='failed',error=?3 WHERE job=?1 AND sequence=?2 AND status='pending' AND EXISTS(SELECT 1 FROM organization_jobs WHERE id=?1 AND state IN('ready','running'))",params![job,seq,format!("{error:#}")])?;
            if changed == 1 {
                tx.execute(
                    "UPDATE organization_jobs SET failed=failed+1,state='paused' WHERE id=?",
                    [job],
                )?;
                event(
                    &tx,
                    job,
                    seq,
                    "failed",
                    &serde_json::json!({"error":format!("{error:#}")}),
                )?;
            }
            tx.commit()?;
            drop(_write);
        }
        self.organization_job(job)
    }
    /// A revised revision or explicit skip is a new recorded decision, never a retry
    /// that quietly adopts data changed since the original selection.
    pub fn review_organization_item(
        &mut self,
        job: &str,
        sequence: i64,
        new_revision: Option<i64>,
    ) -> Result<Job> {
        let _write = self
            .writers
            .enter(crate::catalog_writer::Priority::Foreground)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: String = tx.query_row(
            "SELECT state FROM organization_jobs WHERE id=?",
            [job],
            |r| r.get(0),
        )?;
        ensure!(state == "paused", "only a paused failure can be revised");
        if let Some(revision) = new_revision {
            ensure!(tx.execute("UPDATE organization_job_items SET expected_revision=?3,status='pending',error=NULL WHERE job=?1 AND sequence=?2 AND status='failed'",params![job,sequence,revision])?==1,"failed item not found");
            tx.execute(
                "UPDATE organization_jobs SET failed=failed-1,state='ready' WHERE id=?",
                [job],
            )?;
        } else {
            ensure!(tx.execute("UPDATE organization_job_items SET status='skipped' WHERE job=?1 AND sequence=?2 AND status='failed'",params![job,sequence])?==1,"failed item not found");
            tx.execute("UPDATE organization_jobs SET failed=failed-1,skipped=skipped+1,state=CASE WHEN applied+skipped+1=total THEN 'complete' ELSE 'ready' END WHERE id=?",[job])?;
        }
        event(
            &tx,
            job,
            sequence,
            "review",
            &serde_json::json!({"new_revision":new_revision,"skip":new_revision.is_none()}),
        )?;
        tx.commit()?;
        drop(_write);
        self.organization_job(job)
    }
}
pub(crate) fn page_limit(limit: usize) -> Result<()> {
    ensure!(
        (1..=MAX_BATCH).contains(&limit),
        "page limit must be 1..1000"
    );
    Ok(())
}
fn validate_operation(db: &Connection, op: &Operation) -> Result<()> {
    match op {
        Operation::Rating { value } => ensure!(*value <= 5, "rating must be 0..5"),
        Operation::Label { value } => ensure!(
            value.len() <= 1024 && !value.contains('\0'),
            "invalid label"
        ),
        Operation::AddKeyword { kind, path } | Operation::RemoveKeyword { kind, path } => {
            path_valid(*kind, path)?
        }
        Operation::MoveKeyword { from, to } => {
            path_valid(KeywordKind::Hierarchical, from)?;
            path_valid(KeywordKind::Hierarchical, to)?;
            ensure!(
                from != to && !to.starts_with(from),
                "cannot move a keyword into itself or a descendant"
            );
        }
        Operation::AddCollection { collection } | Operation::RemoveCollection { collection } => {
            ensure!(
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM organization_collections WHERE id=?)",
                    [collection],
                    |r| r.get::<_, bool>(0)
                )?,
                "collection not found"
            )
        }
        _ => {}
    }
    Ok(())
}
fn assert_pending(db: &Connection, job: &str, seq: i64, revision: i64) -> Result<()> {
    ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM organization_job_items j JOIN organization_jobs b ON b.id=j.job JOIN assets a ON a.sequence=j.sequence LEFT JOIN metadata_assets m ON m.asset_id=a.id WHERE j.job=?1 AND j.sequence=?2 AND j.status='pending' AND b.state IN ('ready','running') AND COALESCE(m.revision,0)=?3)",params![job,seq,revision],|r|r.get::<_,bool>(0))?,"batch item, revision, or state changed");
    Ok(())
}
fn event(
    db: &Connection,
    job: &str,
    seq: i64,
    action: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    db.execute(
        "INSERT INTO organization_events(job,sequence,action,detail) VALUES(?1,?2,?3,?4)",
        params![job, seq, action, detail.to_string()],
    )?;
    Ok(())
}
fn complete_item(db: &Connection, job: &str, seq: i64, revision: i64) -> Result<()> {
    ensure!(db.execute("UPDATE organization_job_items SET status='applied',result_revision=?3 WHERE job=?1 AND sequence=?2 AND status='pending'",params![job,seq,revision])?==1,"batch item already completed");
    db.execute("UPDATE organization_jobs SET applied=applied+1,state=CASE WHEN applied+skipped+1=total THEN 'complete' ELSE 'running' END WHERE id=?",[job])?;
    event(
        db,
        job,
        seq,
        "applied",
        &serde_json::json!({"revision":revision}),
    )
}
fn advance_local(db: &Connection, asset: &str, job: &str) -> Result<i64> {
    db.execute("INSERT INTO metadata_assets VALUES(?1,1) ON CONFLICT(asset_id) DO UPDATE SET revision=revision+1",[asset])?;
    let revision: i64 = db.query_row(
        "SELECT revision FROM metadata_assets WHERE asset_id=?",
        [asset],
        |r| r.get(0),
    )?;
    db.execute(
        "INSERT INTO metadata_history(asset_id,revision,action,detail) VALUES(?1,?2,'organize',?3)",
        params![asset, revision, serde_json::json!({"job":job}).to_string()],
    )?;
    Ok(revision)
}
fn organization_edits(
    cat: &Catalog,
    asset: &str,
    op: &Operation,
) -> Result<(Option<i64>, Vec<String>, Vec<Edit>)> {
    let (field, namespace, path) = match op {
        Operation::Rating { .. } => ("rating", xmp::XMP, "Rating"),
        Operation::Label { .. } => ("label", xmp::XMP, "Label"),
        Operation::AddKeyword {
            kind: KeywordKind::Flat,
            ..
        }
        | Operation::RemoveKeyword {
            kind: KeywordKind::Flat,
            ..
        } => ("keywords", xmp::DC, "subject"),
        _ => ("hierarchical_keywords", LR, "hierarchicalSubject"),
    };
    let view = cat.metadata(asset)?;
    let current = view.fields.iter().find(|f| f.name == field);
    ensure!(
        current.is_none_or(|v| !v.conflicted),
        "resolve {field} source conflict before changing organization"
    );
    let base:Option<i64>=cat.db.query_row("SELECT m.id FROM metadata_sources s JOIN metadata_models m ON m.observation_id=s.current_observation WHERE s.asset_id=? AND s.kind='catalog' AND s.locator=X'6C6F63616C2D6D65746164617461' ORDER BY m.id DESC LIMIT 1",[asset],|r|r.get(0)).optional()?;
    let base = base.or(current.and_then(|f| f.selected_model));
    // Reconciliation/canonicalization can sort RDF Bags. Address items in the
    // exact reconciled input used by the subsequent atomic mutation, not in the
    // independently sorted projection or the original packet's physical order.
    let input = cat.organization_edit_input(asset, base, &[field.to_string()])?;
    let meta = xmp::parse(&input)?;
    let values = (1..=meta.array_len(namespace, path))
        .map(|i| {
            meta.array_item(namespace, path, i as i32)
                .map(|v| v.value)
                .context("missing source keyword item")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut edits = Vec::new();
    let set = |path: String, value: String| Edit::Set {
        namespace: namespace.into(),
        path,
        value,
    };
    match op {
        Operation::Rating { value } => edits.push(set(path.into(), value.to_string())),
        Operation::Label { value } => edits.push(set(path.into(), value.clone())),
        Operation::AddKeyword { kind, path: parts } => {
            let term = if *kind == KeywordKind::Flat {
                parts[0].clone()
            } else {
                parts.join("|")
            };
            if !values.contains(&term) {
                edits.push(Edit::Append {
                    namespace: namespace.into(),
                    path: path.into(),
                    value: term,
                    ordered: false,
                });
            }
        }
        Operation::RemoveKeyword { kind, path: parts } => {
            let term = if *kind == KeywordKind::Flat {
                parts[0].clone()
            } else {
                parts.join("|")
            };
            for (i, value) in values.iter().enumerate().rev() {
                if value == &term {
                    edits.push(Edit::Remove {
                        namespace: namespace.into(),
                        path: format!("{path}[{}]", i + 1),
                    });
                }
            }
        }
        Operation::MoveKeyword { from, to } => {
            for (i, value) in values.iter().enumerate() {
                let parts: Vec<String> = value.split('|').map(str::to_owned).collect();
                if parts.starts_with(from) {
                    let moved = to
                        .iter()
                        .chain(parts[from.len()..].iter())
                        .cloned()
                        .collect::<Vec<_>>();
                    path_valid(KeywordKind::Hierarchical, &moved)?;
                    edits.push(set(format!("{path}[{}]", i + 1), moved.join("|")));
                }
            }
        }
        _ => anyhow::bail!("operation has no XMP edit"),
    }
    Ok((base, vec![field.into()], edits))
}

impl Catalog {
    /// One durable organization action; unlike a multi-asset job, this does not
    /// spend extra commits staging an already explicit single-asset selection.
    pub fn organize_asset(
        &mut self,
        asset: &str,
        expected_revision: i64,
        operation: Operation,
    ) -> Result<i64> {
        validate_operation(&self.db, &operation)?;
        ensure!(
            self.render_identity(asset)?.metadata_revision == expected_revision,
            "asset changed before organization edit"
        );
        let seq: i64 =
            self.db
                .query_row("SELECT sequence FROM assets WHERE id=?", [asset], |r| {
                    r.get(0)
                })?;
        match &operation {
            Operation::Flag { .. }
            | Operation::AddCollection { .. }
            | Operation::RemoveCollection { .. } => {
                let _write = self
                    .writers
                    .enter(crate::catalog_writer::Priority::Foreground)?;
                let tx = self
                    .db
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let revision: i64 = tx.query_row(
                    "SELECT COALESCE((SELECT revision FROM metadata_assets WHERE asset_id=?),0)",
                    [asset],
                    |r| r.get(0),
                )?;
                ensure!(
                    revision == expected_revision,
                    "asset changed while preparing organization edit"
                );
                validate_operation(&tx, &operation)?;
                match &operation {
                    Operation::Flag { value } => {
                        tx.execute("INSERT INTO organization_flags VALUES(?1,?2,?3) ON CONFLICT(sequence) DO UPDATE SET flag=excluded.flag,provenance=excluded.provenance",params![seq,value.sql(),serde_json::json!({"operation":operation,"expected_revision":expected_revision}).to_string()])?;
                    }
                    Operation::AddCollection { collection } => {
                        tx.execute("INSERT INTO organization_collection_members VALUES(?1,?2,?3) ON CONFLICT(collection,sequence) DO NOTHING",params![collection,seq,serde_json::json!({"operation":operation,"expected_revision":expected_revision}).to_string()])?;
                        tx.execute(
                            "UPDATE organization_collections SET revision=revision+1 WHERE id=?",
                            [collection],
                        )?;
                    }
                    Operation::RemoveCollection { collection } => {
                        tx.execute("DELETE FROM organization_collection_members WHERE collection=?1 AND sequence=?2",params![collection,seq])?;
                        tx.execute(
                            "UPDATE organization_collections SET revision=revision+1 WHERE id=?",
                            [collection],
                        )?;
                    }
                    _ => unreachable!(),
                }
                let next = advance_local(&tx, asset, "single-asset")?;
                refresh(&tx, asset)?;
                tx.execute("INSERT INTO organization_events(sequence,action,detail) VALUES(?1,'single_asset',?2)",params![seq,serde_json::to_string(&operation)?])?;
                tx.commit()?;
                drop(_write);
                Ok(next)
            }
            _ => {
                let (base, fields, edits) = organization_edits(self, asset, &operation)?;
                Ok(self.edit_metadata_commit(asset,expected_revision,base,&edits,&fields,|db,_|{db.execute("INSERT INTO organization_events(sequence,action,detail) VALUES(?1,'single_asset',?2)",params![seq,serde_json::to_string(&operation)?])?;Ok(())})?.revision)
            }
        }
    }
    pub fn organization_events(&self, after: i64, limit: usize) -> Result<Vec<serde_json::Value>> {
        page_limit(limit)?;
        let mut values = Vec::new();
        for row in self.db.prepare("SELECT id,job,sequence,action,detail,created_at FROM organization_events WHERE id>?1 ORDER BY id LIMIT ?2")?.query_map(params![after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))? {
            let(id,job,sequence,action,detail,created_at)=row?;values.push(serde_json::json!({"id":id,"job":job,"sequence":sequence,"action":action,"detail":serde_json::from_str::<serde_json::Value>(&detail)?,"created_at":created_at}));
        }
        Ok(values)
    }
}
