use super::*;
use crate::organization::{Flag, KeywordKind, Operation};

fn fixture() -> Result<(tempfile::TempDir, Catalog, VariantKey)> {
    let temp = tempfile::tempdir()?;
    let c = Catalog::open(temp.path().join("catalog"))?;
    c.db.execute(
        "INSERT INTO assets(id,location,path_display,state) VALUES('a',X'61','a','pending')",
        [],
    )?;
    crate::organization::refresh(&c.db, "a")?;
    c.db.execute(
        "CREATE TABLE fixture_checkpoint(id INTEGER PRIMARY KEY,revision INTEGER NOT NULL)",
        [],
    )?;
    Ok((temp, c, VariantKey::master("a")))
}

#[test]
fn flag_and_xmp_actions_roll_back_with_failed_checkpoint_then_commit_once() -> Result<()> {
    let (_t, mut c, key) = fixture()?;
    for (i, op) in [
        Operation::Flag { value: Flag::Pick },
        Operation::Rating { value: 4 },
    ]
    .into_iter()
    .enumerate()
    {
        let before = c.image_metadata_identity(&key)?;
        let counts = |db: &Connection| -> Result<(i64, i64, i64)> {
            Ok(db.query_row("SELECT (SELECT count(*) FROM metadata_observations),(SELECT count(*) FROM organization_events),(SELECT count(*) FROM fixture_checkpoint)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
        };
        let old_counts = counts(&c.db)?;
        assert!(
            c.organize_image_with_commit(
                &key,
                before.metadata_revision,
                op.clone(),
                |db, revision| {
                    db.execute(
                        "INSERT INTO fixture_checkpoint VALUES(?1,?2)",
                        params![i as i64, revision],
                    )?;
                    anyhow::bail!("injected checkpoint failure")
                }
            )
            .is_err()
        );
        assert_eq!(c.image_metadata_identity(&key)?, before);
        assert_eq!(counts(&c.db)?, old_counts);
        let revision = c.organize_image_with_commit(
            &key,
            before.metadata_revision,
            op.clone(),
            |db, revision| {
                db.execute(
                    "INSERT INTO fixture_checkpoint VALUES(?1,?2)",
                    params![i as i64, revision],
                )?;
                Ok(())
            },
        )?;
        assert_eq!(revision, before.metadata_revision + 1);
        assert_eq!(
            c.db.query_row(
                "SELECT revision FROM fixture_checkpoint WHERE id=?",
                [i as i64],
                |r| r.get::<_, i64>(0)
            )?,
            revision
        );
        assert!(
            c.organize_image_with_commit(&key, before.metadata_revision, op, |_, _| panic!(
                "stale action must not checkpoint"
            ))
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn dictionary_membership_and_synonym_effects_share_checkpoint_transaction() -> Result<()> {
    let (_t, mut c, key) = fixture()?;
    let expected = c.image_metadata_identity(&key)?;
    let body = |db: &Connection| -> Result<String> {
        let keyword = crate::organization::keyword(
            db,
            KeywordKind::Hierarchical,
            &["birds".into(), "owls".into()],
        )?;
        let collection = crate::organization::create_collection(
            db,
            "Imported",
            &serde_json::json!({"source":"collection"}),
        )?;
        add_keyword_synonym(
            db,
            keyword,
            "Strigiformes",
            &serde_json::json!({"source":"synonym"}),
        )?;
        set_image_collection_membership(
            db,
            &expected,
            &collection,
            4,
            &serde_json::json!({"source":"membership"}),
        )?;
        db.execute("INSERT INTO fixture_checkpoint VALUES(0,1)", [])?;
        Ok(collection)
    };
    {
        let tx = c.db.transaction()?;
        body(&tx)?;
        // A lost checkpoint rolls back dictionaries, membership, revision and callback effects.
    }
    assert_eq!(c.image_metadata_identity(&key)?, expected);
    assert_eq!(c.db.query_row("SELECT (SELECT count(*) FROM organization_keywords)+(SELECT count(*) FROM organization_collections)+(SELECT count(*) FROM fixture_checkpoint)",[],|r|r.get::<_,i64>(0))?,0);
    let collection = {
        let tx = c.db.transaction()?;
        let collection = body(&tx)?;
        tx.commit()?;
        collection
    };
    let members = c.image_collection_members(&collection, None, 10)?;
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].1.key, key);
    assert_eq!(members[0].1.position, 4);
    assert_eq!(
        c.db.query_row(
            "SELECT synonym FROM organization_keyword_synonyms",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "Strigiformes"
    );
    Ok(())
}
