//! Synthetic destination-row tests. No Source process, catalog or original file.
use super::*;
use std::cell::Cell as Count;

#[test]
fn saved_cursor_admits_actual_bytes_and_preserves_large_legacy_spelling() -> Result<()> {
    let db = Connection::open_in_memory()?;
    let body = format!(
        "{}{}",
        " ".repeat(RECORD_LIMIT + 1),
        r#"{"seal":"binding","revision":"revision","collection":"Captures","after":[]}"#
    );
    let seen = Count::new(0);
    let result = db
        .query_row("SELECT ?1", [&body], |r| {
            Ok(admitted_cursor(r, &|n| {
                seen.set(n);
                anyhow::bail!("fixture allocation denied before cursor copy")
            }))
        })?
        .unwrap_err();
    assert!(result.to_string().contains("denied before cursor copy"));
    assert_eq!(seen.get(), body.len());
    let exact = db
        .query_row("SELECT ?1", [&body], |r| {
            Ok(admitted_cursor(r, &|n| {
                seen.set(n);
                Ok(())
            }))
        })??
        .unwrap();
    assert_eq!(exact, body);
    let old: Cursor = serde_json::from_str(&body)?;
    let decoded: Cursor = serde_json::from_str(&exact)?;
    assert_eq!(serde_json::to_vec(&decoded)?, serde_json::to_vec(&old)?);
    for sql in ["SELECT NULL", "SELECT x'61'", "SELECT 17"] {
        let count = Count::new(0);
        let value = db.query_row(sql, [], |r| {
            Ok(admitted_cursor(r, &|_| {
                count.set(count.get() + 1);
                Ok(())
            }))
        })?;
        assert_eq!(count.get(), 0);
        if sql.ends_with("NULL") {
            assert_eq!(value?, None);
        } else {
            assert!(value.is_err());
        }
    }
    println!(
        "CURSOR_ACTUAL_ADMISSION bytes={} exceeds_old_record_cap=true exact_spelling=true denied_before_copy=true",
        body.len()
    );
    Ok(())
}

#[test]
fn pending_record_borrows_compressed_storage_and_admits_cursor_before_decode() -> Result<()> {
    let db = Connection::open_in_memory()?;
    let raw = serde_json::to_vec(&EvidenceRecord {
        revision: "a".repeat(64),
        collection: Collection::Captures,
        rowid: 1,
        key: vec![],
        fields: Default::default(),
    })?;
    let compressed = compress(&raw)?;
    let hash = blake3::hash(&raw).to_hex().to_string();
    let next = "unchanged opaque cursor";
    let seen = Count::new(0);
    let (_, record, actual) = db.query_row(
        "SELECT 7,?1,?2,?3,?4",
        params![compressed, raw.len() as i64, hash, next],
        |r| {
            Ok(pending_record(r, &|n| {
                seen.set(n);
                Ok(())
            }))
        },
    )??;
    assert_eq!(serde_json::to_vec(&record)?, raw);
    assert_eq!(actual, next);
    assert_eq!(seen.get(), next.len());
    // Decoder input is deliberately invalid: admission must win first.
    let error = db
        .query_row("SELECT 7,x'00',1,?1,?2", params![hash, next], |r| {
            Ok(pending_record(r, &|n| {
                assert_eq!(n, next.len());
                anyhow::bail!("fixture denied before decompression")
            }))
        })?
        .unwrap_err();
    assert!(error.to_string().contains("denied before decompression"));
    for (blob, digest) in [
        (vec![0; RECORD_LIMIT + 32769], hash.clone()),
        (compressed.clone(), "short".into()),
    ] {
        let entered = Count::new(false);
        let error = db
            .query_row(
                "SELECT 7,?1,?2,?3,?4",
                params![blob, raw.len() as i64, digest, next],
                |r| {
                    Ok(pending_record(r, &|_| {
                        entered.set(true);
                        Ok(())
                    }))
                },
            )?
            .unwrap_err();
        assert!(!entered.get());
        assert!(!error.to_string().is_empty());
    }
    println!(
        "PENDING_BORROWED_RECORD integrity=true cursor_grant_before_decode=true oversized_and_identity_before_grant=true"
    );
    Ok(())
}

#[test]
fn pending_field_uses_borrowed_name_and_guarded_evidence_identity() -> Result<()> {
    use crate::lightroom::migration_source::ByteRef;
    let db = Connection::open_in_memory()?;
    let reference = ByteRef {
        seal: "a".repeat(64),
        revision: "b".repeat(64),
        collection: Collection::Captures,
        rowid: 1,
        field: "manifest".into(),
        bytes: 1,
        text: false,
    };
    let record = EvidenceRecord {
        revision: "b".repeat(64),
        collection: Collection::Captures,
        rowid: 1,
        key: vec![],
        fields: std::collections::BTreeMap::from([(
            "manifest".into(),
            Field::Bytes(reference.clone()),
        )]),
    };
    let evidence = "c".repeat(64);
    let (got, id, offset) = db.query_row("SELECT 'manifest',?1,0", [&evidence], |r| {
        Ok(pending_field(r, &record))
    })??;
    assert_eq!(serde_json::to_vec(&got)?, serde_json::to_vec(&reference)?);
    assert_eq!(id, evidence);
    assert_eq!(offset, 0);
    let huge = "unknown".repeat(RECORD_LIMIT / 7 + 1);
    let error = db
        .query_row("SELECT ?1,?2,0", params![huge, evidence], |r| {
            Ok(pending_field(r, &record))
        })?
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing retained field descriptor")
    );
    for value in [
        rusqlite::types::Value::Text("short".into()),
        rusqlite::types::Value::Blob(vec![b'c'; 64]),
    ] {
        assert!(
            db.query_row("SELECT 'manifest',?1,0", [value], |r| Ok(pending_field(
                r, &record
            )))?
            .is_err()
        );
    }
    println!("PENDING_FIELD_BORROW borrowed_lookup=true evidence_type_size=true");
    Ok(())
}
