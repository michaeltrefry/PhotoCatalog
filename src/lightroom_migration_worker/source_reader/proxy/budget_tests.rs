//! Full core-size fixtures. These measure Rust-owned payload capacities, not
//! allocator overhead, mapped code, SQLite caches, or process RSS.
use super::tests::session_with_deadline;
use super::*;
use crate::lightroom::{Issue, MANIFEST_BYTES, PAGE_BYTES, migration_source::tests::Fixture};

#[test]
fn maximal_short_issue_manifest_exceeds_old_estimate_and_preserves_every_member() -> Result<()> {
    maximal_issue_manifest(
        br#"{"code":"","detail":""}"#,
        "maximal_short_issue_manifest",
        64 * 1024 * 1024,
    )
}

#[test]
fn maximal_sequence_issue_manifest_preserves_more_members_than_object_case() -> Result<()> {
    maximal_issue_manifest(
        br#"["",null,""]"#,
        "maximal_sequence_issue_manifest",
        128 * 1024 * 1024,
    )
}

fn maximal_issue_manifest(item: &[u8], label: &str, old_capacity_estimate: usize) -> Result<()> {
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
    assert!(issue_capacity_bytes > old_capacity_estimate);
    let encoded = exact_json(
        &Value::Manifest(value),
        RESULT_BYTES,
        &AtomicBool::new(false),
    )?;
    eprintln!(
        "{}",
        serde_json::json!({"fixture":label,"source_bytes":raw.len(),"issues":count,"issue_capacity_bytes":issue_capacity_bytes,"encoded_result_bytes":encoded.len()})
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

#[test]
fn maximal_units_first_manifest_preserves_foreign_evidence_and_rejects_tamper() -> Result<()> {
    let mut fixture = Fixture::new();
    let manifest = fixture.open().capture_manifest(fixture.revision())?;
    let template = serde_json::to_string(&manifest)?;
    let needle = format!(
        "\"output\":{}",
        serde_json::to_string(&manifest.request.output)?
    );
    let at = template.find(&needle).context("output field")?;
    let prefix = format!("{}\"output\":{{\"units\":[", &template[..at]);
    let suffix = format!(
        "],\"encoding\":\"WindowsWide\"}}{}",
        &template[at + needle.len()..]
    );
    let count = (MANIFEST_BYTES - prefix.len() - suffix.len() + 1) / 2;
    let mut raw = Vec::with_capacity(MANIFEST_BYTES);
    raw.extend_from_slice(prefix.as_bytes());
    for index in 0..count {
        if index > 0 {
            raw.push(b',');
        }
        raw.push(b'0');
    }
    raw.extend_from_slice(suffix.as_bytes());
    assert!(raw.len() <= MANIFEST_BYTES && MANIFEST_BYTES - raw.len() < 2);
    let revision = fixture.revision().to_owned();
    fixture.seal.selected[0].manifest_blake3 = crate::lightroom::digest(&raw);
    fixture.edit(|db| {
        db.execute(
            "UPDATE captures SET manifest=? WHERE revision=?",
            rusqlite::params![std::str::from_utf8(&raw).unwrap(), revision],
        )
        .unwrap();
    });
    let before = std::fs::read(&fixture.path)?;
    let remote = maximal_sql(&fixture, ReadLimits::default().inline_bytes)?;
    let value = remote.capture_manifest(&revision)?;
    let crate::storage_volume::NativePath::WindowsWide(units) = &value.request.output else {
        anyhow::bail!("foreign output evidence must retain its units")
    };
    assert_eq!(units.len(), count);
    assert!(units.iter().all(|v| *v == 0));
    eprintln!(
        "{}",
        serde_json::json!({"fixture":"maximal_units_first_manifest","source_bytes":raw.len(),"units":count,"unit_capacity_bytes":units.capacity()*2})
    );
    let mut decoded = value;
    let normalized = serde_json::to_vec(&decoded)?;
    assert_ne!(
        crate::lightroom::digest(&normalized),
        fixture.seal.selected[0].manifest_blake3
    );
    let again = remote.capture_manifest(&revision)?;
    assert_eq!(serde_json::to_vec(&again)?, normalized);
    drop(remote);
    assert_eq!(
        std::fs::read(&fixture.path)?,
        before,
        "read admission must preserve original evidence bytes"
    );
    // Keep the old manifest authority while independently sealing changed DB
    // bytes. The original exact manifest digest, not the normalized view, wins.
    let crate::storage_volume::NativePath::WindowsWide(units) = &mut decoded.request.output else {
        anyhow::bail!("retained foreign units");
    };
    units[0] = 1; // Same-width mutation keeps size admission out of the digest test.
    let tampered = serde_json::to_string(&decoded)?;
    fixture.edit(|db| {
        db.execute(
            "UPDATE captures SET manifest=? WHERE revision=?",
            rusqlite::params![tampered, revision],
        )
        .unwrap();
    });
    let error = match maximal_sql(&fixture, ReadLimits::default().inline_bytes) {
        Ok(_) => anyhow::bail!("tampered manifest admitted"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("manifest differs from seal"), "{error}");
    Ok(())
}

#[test]
fn borrowed_outer_value_checks_method_before_payload_and_preserves_tag_order() -> Result<()> {
    use super::super::wire::Kind;
    let mut fixture = Fixture::new();
    let revision = fixture.revision().to_owned();
    fixture.edit(|db| {
        db.execute(
            "INSERT INTO entities VALUES(?,'image','Adobe_images',NULL,NULL,'{}')",
            [&revision],
        )
        .unwrap();
    });
    let source = fixture.open();
    let revision = revision.as_str();
    let values = [
        (Kind::Verified, Value::Verified),
        (
            Kind::Manifest,
            Value::Manifest(source.capture_manifest(revision)?),
        ),
        (
            Kind::StableSource,
            Value::StableSource(source.stable_source(revision, "lineage-selected:7")?),
        ),
        (
            Kind::OriginPacketRoster,
            Value::OriginPacketRoster(vec![
                crate::application::I64(i64::MIN),
                crate::application::I64(i64::MAX),
            ]),
        ),
        (
            Kind::Page,
            Value::Page(source.page(revision, Collection::Rows, None, 1)?),
        ),
        (Kind::Chunk, Value::Chunk(vec![0, 255])),
        (Kind::Count, Value::Count(U64(u64::MAX))),
        (
            Kind::Resolution,
            Value::Resolution(Resolution::Unique("\0é".into())),
        ),
        (
            Kind::ImageLinks,
            Value::ImageLinks(source.image_links(revision, "image")?),
        ),
    ];
    for (kind, value) in values {
        let encoded = exact_json(&value, RESULT_BYTES, &AtomicBool::new(false))?;
        let decoded = Value::decode(&encoded, kind)?;
        assert_eq!(serde_json::to_vec(&decoded)?, encoded);
        if kind != Kind::Verified {
            #[derive(serde::Deserialize)]
            struct Envelope<'a> {
                kind: String,
                #[serde(borrow)]
                value: &'a serde_json::value::RawValue,
            }
            let raw: Envelope<'_> = serde_json::from_slice(&encoded)?;
            let reversed = format!(
                "{{\"value\":{},\"kind\":{}}}",
                raw.value.get(),
                serde_json::to_string(&raw.kind)?
            );
            assert_eq!(
                serde_json::to_vec(&Value::decode(reversed.as_bytes(), kind)?)?,
                encoded
            );
        }
    }
    // Wrong method must reject at the borrowed header, before any type-specific
    // Manifest parse; the invalid body must not determine the error instead.
    let mismatch = br#"{"value":{"not_a_manifest":[1,2,3]},"kind":"Manifest"}"#;
    assert!(
        Value::decode(mismatch, Kind::Count)
            .unwrap_err()
            .to_string()
            .contains("does not match requested method")
    );
    for invalid in [
        br#"{"kind":"Count","value":"1","kind":"Count"}"#.as_slice(),
        br#"{"kind":"Count","value":"1","value":"1"}"#.as_slice(),
        br#"{"kind":"Count","value":"1","unknown":1}"#.as_slice(),
        br#"{"kind":"Count"}"#.as_slice(),
        br#"{"kind":"Count","value":1}"#.as_slice(),
    ] {
        assert!(Value::decode(invalid, Kind::Count).is_err());
    }
    assert!(matches!(
        Value::decode(br#"{"value":null,"kind":"Verified"}"#, Kind::Verified)?,
        Value::Verified
    ));
    Ok(())
}

#[test]
fn borrowed_envelope_preserves_original_unit_null_and_duplicate_semantics() -> Result<()> {
    use super::super::wire::Kind;
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "kind", content = "value", deny_unknown_fields)]
    enum OriginalValue {
        Verified,
        Count(U64),
    }
    for (kind, raw) in [
        (Kind::Verified, r#"{"kind":"Verified"}"#),
        (Kind::Verified, r#"{"kind":"Verified","value":null}"#),
        (Kind::Verified, r#"{"value":null,"kind":"Verified"}"#),
        (
            Kind::Verified,
            r#"{"kind":"Verified","value":null,"value":null}"#,
        ),
        (
            Kind::Verified,
            r#"{"value":null,"value":null,"kind":"Verified"}"#,
        ),
        (
            Kind::Verified,
            r#"{"kind":"Verified","kind":"Verified","value":null}"#,
        ),
        (Kind::Verified, r#"{"kind":"Verified","value":{}}"#),
        (Kind::Verified, r#"{"kind":"Verified","value":[]}"#),
        (Kind::Verified, r#"{"kind":"Verified","value":0}"#),
        (
            Kind::Verified,
            r#"{"kind":"Verified","value":null,"unknown":null}"#,
        ),
        (Kind::Count, r#"{"kind":"Count"}"#),
        (Kind::Count, r#"{"kind":"Count","value":null}"#),
        (Kind::Count, r#"{"value":null,"kind":"Count"}"#),
        (Kind::Count, r#"{"value":null,"value":"1","kind":"Count"}"#),
        (Kind::Count, r#"{"kind":"Count","value":"1","value":null}"#),
        (
            Kind::Count,
            r#"{"kind":"Count","value":"18446744073709551615"}"#,
        ),
        (
            Kind::Count,
            r#"{"value":"18446744073709551615","kind":"Count"}"#,
        ),
    ] {
        let original = serde_json::from_str::<OriginalValue>(raw);
        let borrowed = Value::decode(raw.as_bytes(), kind);
        assert_eq!(
            original.is_ok(),
            borrowed.is_ok(),
            "raw={raw}; original={original:?}; borrowed={borrowed:?}"
        );
        if let (Ok(original), Ok(borrowed)) = (original, borrowed) {
            assert_eq!(
                serde_json::to_vec(&original)?,
                serde_json::to_vec(&borrowed)?
            );
        }
    }
    Ok(())
}
