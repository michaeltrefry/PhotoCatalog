//! Full core-size fixtures. These measure Rust-owned payload capacities, not
//! allocator overhead, mapped code, SQLite caches, or process RSS.
use super::tests::session_with_deadline;
use super::*;
use crate::lightroom::{Issue, MANIFEST_BYTES, PAGE_BYTES, migration_source::tests::Fixture};

#[test]
fn maximal_short_issue_manifest_exceeds_old_estimate_and_preserves_every_member() -> Result<()> {
    let mut fixture = Fixture::new();
    let manifest = fixture.open().capture_manifest(fixture.revision())?;
    let template = serde_json::to_vec(&manifest)?;
    let marker = b"\"issues\":[]";
    let start = template
        .windows(marker.len())
        .position(|v| v == marker)
        .context("manifest issues field")?;
    let prefix = &template[..start + marker.len() - 1];
    let suffix = &template[start + marker.len() - 1..];
    let item = b"{\"code\":\"\",\"detail\":\"\"}";
    let count = (MANIFEST_BYTES - prefix.len() - suffix.len() + 1) / (item.len() + 1);
    let mut raw = Vec::with_capacity(MANIFEST_BYTES);
    raw.extend_from_slice(prefix);
    for index in 0..count {
        if index != 0 {
            raw.push(b',');
        }
        raw.extend_from_slice(item);
    }
    raw.extend_from_slice(suffix);
    ensure!(raw.len() <= MANIFEST_BYTES, "maximal manifest raw bound");
    ensure!(
        MANIFEST_BYTES - raw.len() < item.len() + 1,
        "fixture must reach core limit"
    );
    let revision = fixture.revision().to_owned();
    fixture.seal.selected[0].manifest_blake3 = crate::lightroom::digest(&raw);
    fixture.edit(|db| {
        db.execute(
            "UPDATE captures SET manifest=? WHERE revision=?",
            rusqlite::params![std::str::from_utf8(&raw).unwrap(), revision],
        )
        .unwrap();
    });
    let remote = maximal_sql(&fixture, ReadLimits::default().inline_bytes)?;
    let value = remote.capture_manifest(&revision)?;
    assert_eq!(value.issues.len(), count);
    assert!(
        value
            .issues
            .iter()
            .all(|i| i.code.is_empty() && i.detail.is_empty() && i.source_id.is_none())
    );
    let issue_capacity_bytes = value
        .issues
        .capacity()
        .checked_mul(std::mem::size_of::<Issue>())
        .context("issue capacity overflow")?;
    // This was the invalid v10 estimate. No allocation ceiling can be inferred
    // from16MiB raw JSON alone or only the currently occupied Vec elements.
    #[cfg(target_pointer_width = "64")]
    assert!(issue_capacity_bytes > 64 * 1024 * 1024);
    let encoded = exact_json(
        &Value::Manifest(value),
        RESULT_BYTES,
        &AtomicBool::new(false),
    )?;
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"maximal_short_issue_manifest","source_bytes":raw.len(),"issues":count,"issue_capacity_bytes":issue_capacity_bytes,"encoded_result_bytes":encoded.len()})
    );
    // Its full result traversed the actual reader, ticket stream and proxy; the
    // original source table remains exactly the admitted retained bytes.
    let source = fixture.open();
    let page = source.page(&revision, Collection::Captures, None, 1)?;
    let crate::lightroom::migration_source::Field::Bytes(reference) =
        &page.records[0].fields["manifest"]
    else {
        anyhow::bail!("full manifest reference");
    };
    assert_eq!(reference.bytes, raw.len() as u64);
    Ok(())
}

#[test]
fn maximal_stable_source_table_and_canonical_key_preserve_expanded_result() -> Result<()> {
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    let inline = PAGE_BYTES / 4;
    let table = "\0".repeat(inline);
    let overhead = serde_json::to_vec(&vec![crate::lightroom::plan::Cell::Blob(vec![])])?.len();
    let elements = (inline - overhead + 1) / 2;
    let key = serde_json::to_vec(&vec![crate::lightroom::plan::Cell::Blob(vec![0; elements])])?;
    assert!(key.len() <= inline && inline - key.len() < 2);
    fixture.edit(|db| {
        db.execute(
            "UPDATE rows SET table_name=?,key_json=? WHERE revision=?",
            rusqlite::params![table, std::str::from_utf8(&key).unwrap(), revision],
        )
        .unwrap();
    });
    let remote = maximal_sql(&fixture, inline)?;
    let value = remote.stable_source(&revision, "lineage-selected:7")?;
    assert_eq!(value.table.as_bytes(), table.as_bytes());
    let crate::lightroom::migration_source::Field::Inline(crate::lightroom::plan::Cell::Text(
        actual,
    )) = &value.source_key
    else {
        anyhow::bail!("canonical key bytes");
    };
    assert_eq!(actual, &key);
    assert_eq!(value.source_key_blake3, crate::lightroom::digest(&key));
    let requested_capacity = value.table.capacity() + actual.capacity();
    let encoded = exact_json(
        &Value::StableSource(value),
        RESULT_BYTES,
        &AtomicBool::new(false),
    )?;
    assert!(
        encoded.len() > MANIFEST_BYTES,
        "fixture must exercise transport overhead beyond16MiB"
    );
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"maximal_stable_source","table_bytes":inline,"canonical_key_bytes":key.len(),"requested_table_key_capacity":requested_capacity,"encoded_result_bytes":encoded.len()})
    );
    Ok(())
}

// Both core execution and transport explicitly use the existing configurable
// maximum. This is a maximum-size acceptance fixture, not a default-latency gate.
fn maximal_sql(fixture: &Fixture, inline_bytes: usize) -> Result<SqlReader> {
    let limits = ReadLimits {
        inline_bytes,
        deadline_ms: 120_000,
        ..ReadLimits::default()
    };
    let session = session_with_deadline(
        Authority::Sql {
            seal: fixture.seal.clone(),
            limits: limits.into(),
            protected: vec![],
        },
        Arc::new(AtomicBool::new(false)),
        limits.deadline_ms,
    )?;
    Ok(SqlReader {
        binding: session.binding.clone(),
        session: RefCell::new(session),
        seal: fixture.seal.clone(),
        chunk_bytes: limits.chunk_bytes,
    })
}

#[test]
fn source_serialization_capacity_is_exact_and_both_passes_are_cancellable() -> Result<()> {
    let cancel = AtomicBool::new(false);
    let value = "\0é".repeat(4097);
    let expected = serde_json::to_vec(&value)?;
    let encoded = exact_json(&value, expected.len(), &cancel)?;
    assert_eq!(encoded, expected);
    assert_eq!(encoded.capacity(), encoded.len());
    assert!(exact_json(&value, expected.len() - 1, &cancel).is_err());
    cancel.store(true, Ordering::Release);
    assert!(exact_json(&value, expected.len(), &cancel).is_err());

    // This synthetic Serialize implementation toggles cancellation in either
    // pass. The production values have no side effects, but writes must still
    // recheck cancellation rather than trusting initial admission.
    struct CancelDuring<'a> {
        pass: std::cell::Cell<usize>,
        cancel_pass: usize,
        cancel: &'a AtomicBool,
    }
    impl serde::Serialize for CancelDuring<'_> {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            let pass = self.pass.get() + 1;
            self.pass.set(pass);
            if pass == self.cancel_pass {
                self.cancel.store(true, Ordering::Release);
            }
            serializer.serialize_str("complete source bytes")
        }
    }
    for cancel_pass in [1, 2] {
        cancel.store(false, Ordering::Release);
        let value = CancelDuring {
            pass: std::cell::Cell::new(0),
            cancel_pass,
            cancel: &cancel,
        };
        assert!(exact_json(&value, 1024, &cancel).is_err());
        assert_eq!(value.pass.get(), cancel_pass);
    }
    Ok(())
}
