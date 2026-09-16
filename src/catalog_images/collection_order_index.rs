//! Derived zero/default membership index. Explicit order/provenance stay intact.
//! Schema installation is constant work; old memberships are covered by bounded,
//! restartable organization maintenance while triggers cover all concurrent writes.
use crate::Catalog;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE organization_collection_zero(collection TEXT NOT NULL,image_sequence INTEGER NOT NULL,
 PRIMARY KEY(collection,image_sequence),
 FOREIGN KEY(collection,image_sequence) REFERENCES organization_collection_members(collection,sequence) ON DELETE CASCADE) WITHOUT ROWID;
CREATE TABLE organization_collection_zero_backfill(id INTEGER PRIMARY KEY CHECK(id=1),
 after_collection TEXT NOT NULL,after_sequence INTEGER NOT NULL,
 high_collection TEXT NOT NULL,high_sequence INTEGER NOT NULL,
 complete INTEGER NOT NULL CHECK(complete IN(0,1)));
CREATE TRIGGER organization_member_zero_insert AFTER INSERT ON organization_collection_members BEGIN
 INSERT OR IGNORE INTO organization_collection_zero
 SELECT new.collection,new.sequence WHERE NOT EXISTS(
 SELECT 1 FROM organization_collection_order WHERE collection=new.collection AND image_sequence=new.sequence AND position>0)
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.sequence);
END;
CREATE TRIGGER organization_member_zero_update AFTER UPDATE OF collection,sequence ON organization_collection_members BEGIN
 DELETE FROM organization_collection_zero WHERE collection=old.collection AND image_sequence=old.sequence;
 INSERT OR IGNORE INTO organization_collection_zero SELECT new.collection,new.sequence
 WHERE NOT EXISTS(SELECT 1 FROM organization_collection_order WHERE collection=new.collection AND image_sequence=new.sequence AND position>0)
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.sequence);
END;
CREATE TRIGGER organization_order_zero_insert AFTER INSERT ON organization_collection_order BEGIN
 DELETE FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.image_sequence;
 INSERT OR IGNORE INTO organization_collection_zero SELECT new.collection,new.image_sequence WHERE new.position=0
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.image_sequence);
END;
CREATE TRIGGER organization_order_zero_update AFTER UPDATE ON organization_collection_order BEGIN
 DELETE FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.image_sequence;
 INSERT OR IGNORE INTO organization_collection_zero SELECT old.collection,old.image_sequence
 WHERE EXISTS(SELECT 1 FROM organization_collection_members WHERE collection=old.collection AND sequence=old.image_sequence)
 AND NOT EXISTS(SELECT 1 FROM organization_collection_order WHERE collection=old.collection AND image_sequence=old.image_sequence AND position>0)
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=old.collection AND image_sequence=old.image_sequence);
 INSERT OR IGNORE INTO organization_collection_zero SELECT new.collection,new.image_sequence WHERE new.position=0
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=new.collection AND image_sequence=new.image_sequence);
END;
CREATE TRIGGER organization_order_zero_delete AFTER DELETE ON organization_collection_order BEGIN
 INSERT OR IGNORE INTO organization_collection_zero SELECT old.collection,old.image_sequence
 WHERE EXISTS(SELECT 1 FROM organization_collection_members WHERE collection=old.collection AND sequence=old.image_sequence)
 AND NOT EXISTS(SELECT 1 FROM organization_collection_zero WHERE collection=old.collection AND image_sequence=old.image_sequence);
END;
"#;
pub(crate) const CANDIDATES: &str = "SELECT m.collection,m.sequence,COALESCE(o.position,0) FROM organization_collection_members m LEFT JOIN organization_collection_order o ON o.collection=m.collection AND o.image_sequence=m.sequence WHERE (m.collection,m.sequence)>(?1,?2) AND (m.collection,m.sequence)<=(?3,?4) ORDER BY m.collection,m.sequence LIMIT ?5";

pub(crate) fn install(db: &Connection) -> Result<()> {
    ensure!(
        !db.is_autocommit(),
        "collection order index migration requires transaction"
    );
    db.execute_batch(SCHEMA)?;
    let high:Option<(String,i64)>=db.query_row("SELECT collection,sequence FROM organization_collection_members ORDER BY collection DESC,sequence DESC LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let complete = high.is_none();
    let (collection, sequence) = high.unwrap_or_default();
    db.execute(
        "INSERT INTO organization_collection_zero_backfill VALUES(1,'',0,?1,?2,?3)",
        params![collection, sequence, complete],
    )?;
    Ok(())
}
pub(crate) fn ready(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT complete FROM organization_collection_zero_backfill WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
pub(crate) fn step(db: &Connection, limit: usize) -> Result<usize> {
    ensure!(
        !db.is_autocommit() && limit <= 1000,
        "collection order index maintenance bounds"
    );
    if limit == 0 || ready(db)? {
        return Ok(0);
    }
    let (after,sequence,high,end):(String,i64,String,i64)=db.query_row("SELECT after_collection,after_sequence,high_collection,high_sequence FROM organization_collection_zero_backfill WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let rows = db
        .prepare(CANDIDATES)?
        .query_map(params![after, sequence, high, end, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (collection, sequence, position) in &rows {
        if *position == 0 {
            db.execute(
                "INSERT OR IGNORE INTO organization_collection_zero VALUES(?1,?2)",
                params![collection, sequence],
            )?;
        } else {
            db.execute("DELETE FROM organization_collection_zero WHERE collection=?1 AND image_sequence=?2",params![collection,sequence])?;
        }
    }
    let (after, sequence) = rows
        .last()
        .map(|v| (v.0.as_str(), v.1))
        .unwrap_or((&high, end));
    let complete = rows.len() < limit || (after, sequence) == (high.as_str(), end);
    db.execute("UPDATE organization_collection_zero_backfill SET after_collection=?1,after_sequence=?2,complete=?3 WHERE id=1",params![after,sequence,complete])?;
    Ok(rows.len())
}
impl Catalog {
    /// False means the durable old-catalog projection is still preparing. An empty
    /// projection must not be interpreted as an empty collection during that time.
    pub fn collection_order_index_ready(&self) -> Result<bool> {
        ready(&self.db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base(db: &Connection) -> Result<()> {
        db.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE organization_collections(id TEXT PRIMARY KEY);
            INSERT INTO organization_collections VALUES('c');
            CREATE TABLE organization_collection_members(collection TEXT,sequence INTEGER,provenance TEXT,PRIMARY KEY(collection,sequence));
            CREATE TABLE organization_collection_order(collection TEXT,image_sequence INTEGER,position INTEGER,
            PRIMARY KEY(collection,image_sequence),FOREIGN KEY(collection,image_sequence) REFERENCES organization_collection_members(collection,sequence) ON DELETE CASCADE);
            CREATE INDEX organization_collection_order_page ON organization_collection_order(collection,position,image_sequence);
            WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<40)
            INSERT INTO organization_collection_members SELECT 'c',x,'exact provenance' FROM n;
            INSERT INTO organization_collection_order SELECT collection,sequence,CASE WHEN sequence%3=0 THEN 0 ELSE sequence END FROM organization_collection_members WHERE sequence%2=0;")?;
        Ok(())
    }
    fn equivalent(db: &Connection) -> Result<()> {
        let actual=db.prepare("SELECT collection,image_sequence FROM organization_collection_zero ORDER BY collection,image_sequence")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let expected=db.prepare("SELECT m.collection,m.sequence FROM organization_collection_members m LEFT JOIN organization_collection_order o ON o.collection=m.collection AND o.image_sequence=m.sequence WHERE COALESCE(o.position,0)=0 ORDER BY m.collection,m.sequence")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(actual, expected);
        Ok(())
    }
    #[test]
    fn migration_is_bounded_restartable_and_triggers_cover_concurrent_authoritative_changes()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("index.sqlite3");
        let mut db = Connection::open(&file)?;
        base(&db)?;
        {
            let tx = db.transaction()?;
            install(&tx)?;
            tx.commit()?;
        }
        assert!(!ready(&db)?);
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM organization_collection_zero",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0,
            "installation must not backfill synchronously"
        );
        {
            let tx = db.transaction()?;
            assert_eq!(step(&tx, 7)?, 7);
            tx.commit()?;
        }
        drop(db);
        let mut db = Connection::open(&file)?;
        db.pragma_update(None, "foreign_keys", true)?;
        assert!(!ready(&db)?);
        assert_eq!(
            db.query_row(
                "SELECT after_sequence FROM organization_collection_zero_backfill",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            7
        );
        db.execute_batch("INSERT INTO organization_collection_members VALUES('c',50,'new');
            INSERT INTO organization_collection_order VALUES('c',50,9);
            UPDATE organization_collection_order SET position=0 WHERE collection='c' AND image_sequence=2;
            DELETE FROM organization_collection_order WHERE collection='c' AND image_sequence=4;
            DELETE FROM organization_collection_members WHERE collection='c' AND sequence=6;")?;
        while !ready(&db)? {
            let tx = db.transaction()?;
            assert!(step(&tx, 7)? <= 7);
            tx.commit()?;
        }
        equivalent(&db)?;
        assert_eq!(db.query_row("SELECT count(*) FROM organization_collection_members WHERE provenance='exact provenance'",[],|r|r.get::<_,i64>(0))?,39);
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM organization_collection_order WHERE image_sequence=3",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0,
            "implicit order absence must stay absent"
        );
        db.execute_batch("INSERT INTO organization_collection_members SELECT collection,60,provenance FROM organization_collection_members WHERE sequence=2;
            INSERT INTO organization_collection_order SELECT collection,60,position FROM organization_collection_order WHERE image_sequence=2;
            UPDATE organization_collection_order SET position=7 WHERE image_sequence=60;
            DELETE FROM organization_collection_order WHERE image_sequence=60;")?;
        equivalent(&db)?;
        db.execute(
            "UPDATE organization_collection_members SET sequence=61 WHERE sequence=60",
            [],
        )?;
        equivalent(&db)?;
        {
            let tx = db.transaction()?;
            tx.execute(
                "INSERT INTO organization_collection_members VALUES('c',99,'rolled back')",
                [],
            )?;
            tx.rollback()?;
        }
        equivalent(&db)?;
        assert!(ready(&db)?);
        drop(db);
        assert!(ready(&Connection::open(&file)?)?);
        Ok(())
    }
    #[test]
    fn backfill_uses_bounded_primary_key_candidates_without_sort() -> Result<()> {
        let db = Connection::open_in_memory()?;
        base(&db)?;
        let plans = db
            .prepare(&format!("EXPLAIN QUERY PLAN {CANDIDATES}"))?
            .query_map(params!["", 0, "c", 40, 7], |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            plans.iter().all(|v| !v.contains("TEMP B-TREE")),
            "{plans:?}"
        );
        assert!(
            plans
                .iter()
                .any(|v| v.contains("SEARCH m USING COVERING INDEX")),
            "{plans:?}"
        );
        let rows = db
            .prepare(CANDIDATES)?
            .query_map(params!["c", 7, "c", 40, 7], |r| r.get::<_, i64>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(rows, (8..=14).collect::<Vec<_>>());
        Ok(())
    }
}
