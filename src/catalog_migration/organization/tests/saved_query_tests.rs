use super::*;

#[test]
fn saved_organization_queries_bound_borrowed_results_and_preserve_replay() -> Result<()> {
    let mut b = Bed::new(false)?;
    let request = b.request(
        100,
        Decision::Collection {
            name: "A".into(),
            parent: None,
            position: 100,
            decision: DictionaryDecision::Create,
        },
    );
    let expected = b
        .catalog
        .project_migration_organization(Some(&b.source), &request)?;
    let identity = request.origin.source.identity()?;
    let check = |b: &Bed| -> Result<()> {
        assert_eq!(
            existing(&b.catalog.db, &request, &expected.input_digest)?,
            Some(expected.clone())
        );
        assert_eq!(
            mapped(&b.catalog.db, &request.import_source, &request.origin)?,
            expected.target
        );
        assert_eq!(
            b.catalog
                .migration_organization_projection(&request.origin.source, "dictionary")?,
            Some(expected.clone())
        );
        Ok(())
    };
    check(&b)?;
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
        let replay = existing(&b.catalog.db, &request, &expected.input_digest).unwrap_err();
        let dictionary =
            mapped(&b.catalog.db, &request.import_source, &request.origin).unwrap_err();
        let public = b
            .catalog
            .migration_organization_projection(&request.origin.source, "dictionary")
            .unwrap_err();
        if expression == "result || ' '" {
            assert!(
                replay
                    .to_string()
                    .contains("stored organization result limit")
            );
            assert!(
                dictionary
                    .to_string()
                    .contains("dictionary mapping size limit")
            );
            assert!(public.to_string().contains("organization result limit"));
        }
        b.catalog
            .db
            .execute_batch("ROLLBACK TO bad_result; RELEASE bad_result")?;
        check(&b)?;
    }
    for column in ["owner", "adapter", "input_digest"] {
        for expression in [
            "hex(zeroblob(524288))",
            "CAST(x'ff' AS TEXT)",
            "zeroblob(64)",
        ] {
            b.catalog.db.execute_batch("SAVEPOINT bad_identity")?;
            // Invalid JSON also proves identity rejection precedes result decode.
            b.catalog.db.execute(&format!("UPDATE migration_organization SET {column}={expression}, result='{{' WHERE source_identity=?"), [&identity])?;
            let error = existing(&b.catalog.db, &request, &expected.input_digest).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("organization mapping decision changed")
            );
            b.catalog
                .db
                .execute_batch("ROLLBACK TO bad_identity; RELEASE bad_identity")?;
            check(&b)?;
        }
    }
    assert_eq!(
        b.catalog.project_migration_organization(None, &request)?,
        expected
    );
    Ok(())
}
