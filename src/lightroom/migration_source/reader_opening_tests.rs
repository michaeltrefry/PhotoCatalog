use super::*;
use crate::lightroom::migration_source::tests::Fixture;
use serde::de::DeserializeOwned;

#[test]
fn opening_columns_reject_oversize_bytes_and_wrong_storage_before_copy() -> Result<()> {
    let cases = [
        (
            "UPDATE captures SET revision=replace(hex(zeroblob(33)),'0','é') WHERE revision=(SELECT min(revision) FROM captures)",
            "capture revision storage",
        ),
        (
            "UPDATE captures SET revision=zeroblob(64) WHERE revision=(SELECT min(revision) FROM captures)",
            "capture revision storage",
        ),
        (
            "UPDATE family_choices SET revision=replace(hex(zeroblob(33)),'0','é')",
            "family revision storage",
        ),
        (
            "UPDATE family_choices SET evidence_digest=zeroblob(64)",
            "family evidence digest storage",
        ),
        (
            "UPDATE family_choices SET evidence_digest=replace(hex(zeroblob(33)),'0','é')",
            "family evidence digest storage",
        ),
        (
            "UPDATE family_choices SET reason=replace(hex(zeroblob(1025)),'0','é')",
            "family reason storage",
        ),
        (
            "UPDATE family_choices SET reason=zeroblob(1)",
            "family reason storage",
        ),
        (
            "UPDATE captures SET stage=replace(hex(zeroblob(4096)),'0','é')",
            "selected inspection not complete",
        ),
    ];
    for (sql, expected) in cases {
        let mut fixture = Fixture::new();
        fixture.edit(|db| {
            db.execute(sql, []).unwrap();
        });
        let before = std::fs::read(&fixture.path)?;
        let result = MigrationSource::open(fixture.seal.clone(), ReadLimits::default());
        let error = match result {
            Err(error) => format!("{error:#}"),
            Ok(_) => anyhow::bail!("accepted {sql}"),
        };
        assert!(error.contains(expected), "{sql}: {error}");
        assert_eq!(std::fs::read(&fixture.path)?, before);
    }
    let mut fixture = Fixture::new();
    fixture.edit(|db| {
        db.execute("UPDATE family_choices SET reason=?1", ["é".repeat(2048)])
            .unwrap();
    });
    drop(fixture.open());
    fixture.edit(|db| {
        db.execute(
            "UPDATE family_choices SET reason=?1",
            ["\u{2003}".repeat(10)],
        )
        .unwrap();
    });
    assert!(MigrationSource::open(fixture.seal, ReadLimits::default()).is_err());
    Ok(())
}

fn parity<T: DeserializeOwned + std::fmt::Debug + PartialEq>(value: &serde_json::Value) {
    let old = serde_json::from_value::<T>(value.clone());
    let borrowed = T::deserialize(value);
    assert_eq!(
        old.is_ok(),
        borrowed.is_ok(),
        "{value}: {old:?} / {borrowed:?}"
    );
    if let (Ok(old), Ok(borrowed)) = (old, borrowed) {
        assert_eq!(old, borrowed);
    }
}

#[test]
fn supplement_borrowed_revision_and_status_preserve_value_semantics() -> Result<()> {
    use crate::xmp_packets::{SourceRevision, Status};
    for raw in [
        r#"{"length":18446744073709551615,"blake3":"retained","modified_unix_ns":18446744073709551615}"#,
        r#"{"length":1,"blake3":"first","blake3":"last","extra":{"nested":[1,null,{}]}}"#,
        r#"{"length":1,"blake3":"retained","modified_unix_ns":null}"#,
        r#"[1,"retained",null]"#,
        r#"{"length":-1,"blake3":"retained"}"#,
        r#"{"length":1,"blake3":{},"modified_unix_ns":0}"#,
        r#"{"length":1,"blake3":"retained","modified_unix_ns":340282366920938463463374607431768211455}"#,
        "null",
        "[]",
        "0",
    ] {
        let value: serde_json::Value = serde_json::from_str(raw)?;
        parity::<SourceRevision>(&value);
    }
    for raw in [
        r#""Absent""#,
        r#""Complete""#,
        r#""Unsupported""#,
        r#""Malformed""#,
        r#""ResourceLimit""#,
        r#""SourceChanged""#,
        r#"{"Complete":null}"#,
        r#"{"Complete":{},"Absent":null}"#,
        r#""unknown""#,
        "null",
        "[]",
        "1",
    ] {
        parity::<Status>(&serde_json::from_str(raw)?);
    }
    Ok(())
}

#[test]
fn opening_index_definition_compares_in_sql_without_loading_retained_sql() -> Result<()> {
    let mut fixture = Fixture::new();
    fixture.edit(|db| {
        let (name, sql): (String, String) = db.query_row(
            "SELECT name,sql FROM sqlite_schema WHERE type='index' AND name='rows_revision_sequence'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(name, "rows_revision_sequence");
        // Same indexed columns and predicate, but not the exact installed
        // definition. Only this synthetic fixture constructs the long string.
        let altered = sql.replacen(" ON ", &format!(" /*{}*/ ON ", "é".repeat(4097)), 1);
        assert_ne!(altered, sql);
        db.execute_batch(&format!("DROP INDEX {name}; {altered};")).unwrap();
    });
    let error = match MigrationSource::open(fixture.seal, ReadLimits::default()) {
        Err(error) => error,
        Ok(_) => anyhow::bail!("accepted replaced index"),
    };
    assert!(format!("{error:#}").contains("incompatible inspection paging index"));
    Ok(())
}
