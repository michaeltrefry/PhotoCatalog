use super::*;

#[test]
fn dictionary_saved_query_checks_borrowed_owner_and_result_before_decode() -> Result<()> {
    let mut b = Bed::new(false)?;
    let origin = b.row(0, 1)?;
    project(
        &mut b.catalog,
        &b.source,
        &b.policy,
        &origin,
        Stage::Keywords,
    )?;
    let expected = existing_slot(&b.catalog, &b.policy, &origin, "dictionary")?.unwrap();
    let identity = origin.source.identity()?;
    let check = |b: &Bed| -> Result<()> {
        assert_eq!(
            existing_slot(&b.catalog, &b.policy, &origin, "dictionary")?,
            Some(expected.clone())
        );
        Ok(())
    };
    let mut boundary = serde_json::to_string(&expected)?;
    boundary.push_str(&" ".repeat(65536 - boundary.len()));
    b.catalog.db.execute(
        "UPDATE migration_organization SET result=? WHERE source_identity=?",
        params![boundary, identity],
    )?;
    check(&b)?;
    for expression in [
        "result || ' '",
        "CAST(result AS BLOB)",
        "CAST(x'ff' AS TEXT)",
        "'{}'",
    ] {
        b.catalog.db.execute_batch("SAVEPOINT bad_result")?;
        b.catalog.db.execute(
            &format!(
                "UPDATE migration_organization SET result={expression} WHERE source_identity=?"
            ),
            [&identity],
        )?;
        let error = existing_slot(&b.catalog, &b.policy, &origin, "dictionary").unwrap_err();
        if expression == "result || ' '" {
            assert!(
                error
                    .to_string()
                    .contains("dictionary owner/result differs")
            );
        }
        b.catalog
            .db
            .execute_batch("ROLLBACK TO bad_result; RELEASE bad_result")?;
        check(&b)?;
    }
    for column in ["owner", "adapter"] {
        for expression in [
            "hex(zeroblob(524288))",
            "CAST(x'ff' AS TEXT)",
            "zeroblob(64)",
        ] {
            b.catalog.db.execute_batch("SAVEPOINT bad_identity")?;
            b.catalog.db.execute(&format!("UPDATE migration_organization SET {column}={expression}, result='{{' WHERE source_identity=?"), [&identity])?;
            let error = existing_slot(&b.catalog, &b.policy, &origin, "dictionary").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("dictionary owner/result differs")
            );
            b.catalog
                .db
                .execute_batch("ROLLBACK TO bad_identity; RELEASE bad_identity")?;
            check(&b)?;
        }
    }
    assert!(existing_slot(&b.catalog, &b.policy, &origin, "missing")?.is_none());
    Ok(())
}
