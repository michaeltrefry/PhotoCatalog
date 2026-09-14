use super::super::{Kind, Value};
use super::*;
use crate::application::U64;
const REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BINDING: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
fn decode(value: &Value, read: &Read, budget: Budget) -> Result<Value> {
    let bytes = serde_json::to_vec(value)?;
    Value::decode_checked(
        &bytes,
        Expected {
            read,
            budget,
            binding: BINDING,
        },
        &|| false,
    )
}
fn sql() -> Budget {
    Budget::Sql(ReadLimits::default())
}
#[test]
fn all_ten_collection_pages_keep_complete_named_fields_and_exact_capacities() -> Result<()> {
    for collection in [
        Collection::Captures,
        Collection::Rows,
        Collection::Entities,
        Collection::References,
        Collection::Paths,
        Collection::Packets,
        Collection::MetadataFacts,
        Collection::Issues,
        Collection::Tables,
        Collection::SchemaObjects,
    ] {
        let (keys, names, numeric) = collection.transport_shape();
        let key: Vec<_> = keys
            .iter()
            .map(|_| {
                if numeric {
                    Cell::Integer(1)
                } else {
                    Cell::Text(vec![0, b'x'])
                }
            })
            .collect();
        let fields = names
            .iter()
            .enumerate()
            .map(|(n, name)| {
                (
                    (*name).into(),
                    match n % 4 {
                        0 => Field::Inline(Cell::Text(vec![0, 255, b'x'])),
                        1 => Field::Inline(Cell::Blob(vec![1, 255, 0])),
                        2 => Field::Inline(Cell::RealBits(u64::MAX)),
                        _ => Field::Bytes(ByteRef {
                            seal: BINDING.into(),
                            revision: REV.into(),
                            collection,
                            rowid: 1,
                            field: (*name).into(),
                            bytes: u64::MAX,
                            text: true,
                        }),
                    },
                )
            })
            .collect();
        let page = Value::Page(Page {
            records: vec![EvidenceRecord {
                revision: REV.into(),
                collection,
                rowid: 1,
                key: key.clone(),
                fields,
            }],
            next: Some(Cursor {
                seal: BINDING.into(),
                revision: REV.into(),
                collection,
                after: key,
            }),
            exhausted: true,
        });
        let read = Read::Sql(Query::Page {
            revision: REV.into(),
            collection,
            after: None,
            limit: U64(1),
        });
        let result = decode(&page, &read, sql())?;
        assert_eq!(serde_json::to_vec(&result)?, serde_json::to_vec(&page)?);
        let Value::Page(result) = result else {
            panic!("page changed kind")
        };
        assert_eq!(result.records.capacity(), 1);
        assert_eq!(result.records[0].key.capacity(), keys.len());
        assert_eq!(result.records[0].fields.len(), names.len());
        let serialized = serde_json::to_string(&page)?;
        let name = names[0];
        let needle = "\"fields\":{";
        let duplicate_valid = serialized.replacen(
            needle,
            &format!("{needle}\"{name}\":{{\"Inline\":{{\"type\":\"Null\"}}}},"),
            1,
        );
        let decoded = Value::decode_checked(
            duplicate_valid.as_bytes(),
            Expected {
                read: &read,
                budget: sql(),
                binding: BINDING,
            },
            &|| false,
        )?;
        assert_eq!(serde_json::to_vec(&decoded)?, serde_json::to_vec(&page)?);
        let duplicate_invalid = serialized.replacen(
            needle,
            &format!("{needle}\"{name}\":{{\"Inline\":{{\"type\":\"Text\",\"value\":\"gg\"}}}},"),
            1,
        );
        assert!(
            Value::decode_checked(
                duplicate_invalid.as_bytes(),
                Expected {
                    read: &read,
                    budget: sql(),
                    binding: BINDING
                },
                &|| false
            )
            .is_err()
        );
        // Exact canonical admission includes cursor, framing and separators.
        let Value::Page(ref body) = page else {
            unreachable!()
        };
        let canonical = size(body, crate::lightroom::PAGE_BYTES)?;
        let mut limits = ReadLimits::default();
        limits.page_bytes = canonical;
        assert!(decode(&page, &read, Budget::Sql(limits)).is_ok());
        limits.page_bytes = canonical - 1;
        assert!(decode(&page, &read, Budget::Sql(limits)).is_err());

        // Every collection rejects an out-of-roster field before parsing its body.
        let mut wrong = serde_json::to_value(&page)?;
        wrong["value"]["records"][0]["fields"]["not_a_spec_field"] =
            serde_json::json!({"Inline":{"type":"Blob","value":"zz"}});
        assert!(
            Value::decode_checked(
                &serde_json::to_vec(&wrong)?,
                Expected {
                    read: &read,
                    budget: sql(),
                    binding: BINDING
                },
                &|| false
            )
            .is_err()
        );
    }
    Ok(())
}
#[test]
fn all_nine_kinds_apply_expected_query_and_full_scalar_bounds() -> Result<()> {
    let rev = REV.to_string();
    let sid = "source".to_string();
    let stable = StableSource {
        capture_revision: rev.clone(),
        table: "Adobe_images".into(),
        source_key: Field::Inline(Cell::Text(b"[]".to_vec())),
        source_key_blake3: BINDING.into(),
        inspection_source_id: sid.clone(),
    };
    let manifest = crate::lightroom::capture::Manifest {
        protocol: 0,
        request: crate::lightroom::capture::Request {
            source: crate::storage_volume::NativePath::UnixBytes(vec![]),
            output: crate::storage_volume::NativePath::WindowsWide(vec![55296, 0]),
            include_auxiliary: true,
            closed_application_evidence: None,
            limits: crate::lightroom::Limits::default(),
        },
        state: String::new(),
        raw_byte_retention: String::new(),
        sqlite_consistency: String::new(),
        application_consistency: String::new(),
        cooperative_lock_protocol: String::new(),
        artifacts: vec![],
        companion_inventory: vec![],
        absent_companions: vec![],
        issues: vec![],
        wal: None,
        logical_blake3: None,
        logical_revision: None,
        revision_id: None,
    };
    #[derive(serde::Serialize)]
    struct PriorManifestEnvelope<'a> {
        kind: &'static str,
        value: &'a crate::lightroom::capture::Manifest,
    }
    let prior_manifest_bytes = serde_json::to_vec(&PriorManifestEnvelope {
        kind: "Manifest",
        value: &manifest,
    })?;
    let manifest_value = Value::Manifest(Box::new(manifest));
    assert_eq!(serde_json::to_vec(&manifest_value)?, prior_manifest_bytes);
    println!(
        "manifest Value fixed boxed allocation={} bytes; Value inline size={} bytes",
        std::mem::size_of::<crate::lightroom::capture::Manifest>(),
        std::mem::size_of::<Value>(),
    );
    let cases = vec![
        (
            manifest_value,
            Read::Sql(Query::CaptureManifest {
                revision: rev.clone(),
            }),
            sql(),
        ),
        (
            Value::StableSource(stable),
            Read::Sql(Query::StableSource {
                revision: rev.clone(),
                source_id: sid.clone(),
            }),
            sql(),
        ),
        (
            Value::OriginPacketRoster(vec![I64(i64::MIN), I64(0), I64(i64::MAX)]),
            Read::Sql(Query::OriginPacketRoster {
                revision: rev.clone(),
                source_id: sid.clone(),
                origin: "embedded".into(),
            }),
            sql(),
        ),
        (
            Value::Chunk(vec![0, 255, 1]),
            Read::Sql(Query::ReadChunk {
                reference: ByteRef {
                    seal: BINDING.into(),
                    revision: rev.clone(),
                    collection: Collection::Rows,
                    rowid: 1,
                    field: "cells_json".into(),
                    bytes: 3,
                    text: true,
                },
                offset: U64(0),
                limit: U64(3),
            }),
            sql(),
        ),
        (
            Value::Chunk(vec![0, 255]),
            Read::ArtifactChunk { offset: U64(0) },
            Budget::Raw(2),
        ),
        (
            Value::Count(U64(u64::MAX)),
            Read::Sql(Query::Count {
                revision: rev.clone(),
                collection: Collection::Rows,
            }),
            sql(),
        ),
        (
            Value::Resolution(Resolution::Unique(String::new())),
            Read::Sql(Query::Resolve {
                revision: rev.clone(),
                source_id: sid.clone(),
                field: "rootFile".into(),
                target_table: "AgLibraryFile".into(),
            }),
            sql(),
        ),
        (
            Value::ImageLinks(ImageLinks {
                image_source_id: sid.clone(),
                file: Resolution::Missing,
                master: Resolution::Ambiguous,
                current_develop: Resolution::Unique("source".into()),
                limitations: source::IMAGE_LINK_LIMITATIONS.into(),
            }),
            Read::Sql(Query::ImageLinks {
                revision: rev,
                source_id: sid,
            }),
            sql(),
        ),
        (Value::Verified, Read::ArtifactVerify, Budget::Raw(1)),
    ];
    for (value, read, budget) in cases {
        let out = decode(&value, &read, budget)?;
        assert_eq!(serde_json::to_vec(&out)?, serde_json::to_vec(&value)?);
    }
    // The collection matrix exercises Page, the ninth kind, through this same entry.
    let read = Read::Sql(Query::Count {
        revision: REV.into(),
        collection: Collection::Rows,
    });
    for raw in [
        br#"{"kind":"Chunk","value":[0]}"#.as_slice(),
        br#"{"kind":"Count","value":"18446744073709551616"}"#,
    ] {
        assert!(
            Value::decode_checked(
                raw,
                Expected {
                    read: &read,
                    budget: sql(),
                    binding: BINDING
                },
                &|| false
            )
            .is_err()
        );
    }
    for raw in [
        br#"{"kind":"Verified"}"#.as_slice(),
        br#"{"value":null,"kind":"Verified"}"#,
    ] {
        assert!(matches!(
            Value::decode_checked(
                raw,
                Expected {
                    read: &Read::ArtifactVerify,
                    budget: Budget::Raw(1),
                    binding: BINDING
                },
                &|| false
            )?,
            Value::Verified
        ));
    }
    assert_eq!(Read::ArtifactVerify.expected_kind(), Kind::Verified);
    Ok(())
}
#[test]
fn typed_cell_representation_parity_has_explicit_positive_and_negative_outcomes() -> Result<()> {
    for (raw, accepted) in [
        (r#"{"type":"Null"}"#, true),
        (r#"["Null",null]"#, true),
        (r#"["Null"]"#, false),
        (r#"{"value":null,"type":"Null"}"#, true),
        (r#"{"type":"Integer","value":-9223372036854775808}"#, true),
        (r#"{"type":"RealBits","value":18446744073709551615}"#, true),
        (r#"{"value":"00fF","type":{"Text":null}}"#, true),
        (r#"["Text","00ff"]"#, true),
        (r#"["Blob",""]"#, true),
        (r#"[{"Text":null},""]"#, false),
        (r#"[0,""]"#, false),
        (r#"{"type":"Text","value":"0"}"#, false),
        (r#"{"type":"Text","value":"gg"}"#, false),
        (r#"{"type":"Integer","value":9223372036854775808}"#, false),
        (r#"{"type":"Text","value":"","value":""}"#, false),
    ] {
        let original = serde_json::from_str::<Cell>(raw);
        let projected = cell(serde_json::from_str(raw)?, 4);
        assert_eq!(original.is_ok(), accepted, "public {raw}");
        assert_eq!(projected.is_ok(), accepted, "private {raw}");
        if let (Ok(a), Ok(b)) = (original, projected) {
            assert_eq!(a, b);
        }
    }
    Ok(())
}
#[test]
fn malformed_budget_bodies_cancel_and_reject_before_repeated_output_allocation() -> Result<()> {
    let read = Read::ArtifactChunk { offset: U64(0) };
    let value = Value::Chunk(vec![0; 1024 * 1024]);
    let bytes = serde_json::to_vec(&value)?;
    let out = Value::decode_checked(
        &bytes,
        Expected {
            read: &read,
            budget: Budget::Raw(1024 * 1024),
            binding: BINDING,
        },
        &|| false,
    )?;
    let Value::Chunk(out) = out else {
        panic!("chunk")
    };
    assert_eq!(out.len(), out.capacity());
    assert!(
        Value::decode_checked(
            &bytes,
            Expected {
                read: &read,
                budget: Budget::Raw(1024 * 1024 - 1),
                binding: BINDING
            },
            &|| false
        )
        .is_err()
    );
    let calls = std::cell::Cell::new(0usize);
    assert!(
        Value::decode_checked(
            &bytes,
            Expected {
                read: &read,
                budget: Budget::Raw(1024 * 1024),
                binding: BINDING
            },
            &|| {
                calls.set(calls.get() + 1);
                calls.get() > 64
            }
        )
        .is_err()
    );
    let roster = Value::OriginPacketRoster(vec![I64(0); 2049]);
    let read = Read::Sql(Query::OriginPacketRoster {
        revision: REV.into(),
        source_id: "source".into(),
        origin: "embedded".into(),
    });
    assert!(decode(&roster, &read, sql()).is_err());
    Ok(())
}
