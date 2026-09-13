use super::*;
use crate::{catalog_metadata, xmp_packets};
use anyhow::Result;
use std::collections::BTreeMap;
#[path = "metadata_import_fixture.rs"]
mod imported;
fn fixture() -> Result<(tempfile::TempDir, Catalog, VariantKey)> {
    let temp = tempfile::tempdir()?;
    let mut catalog = Catalog::open(temp.path().join("catalog"))?;
    let key = asset(&mut catalog, "synthetic")?;
    Ok((temp, catalog, key))
}
fn asset(c: &mut Catalog, name: &str) -> Result<VariantKey> {
    c.db.execute(
        "INSERT INTO assets(id,location,path_display,state) VALUES(?1,?2,?1,'pending')",
        params![name, name.as_bytes()],
    )?;
    crate::organization::refresh(&c.db, name)?;
    Ok(VariantKey::master(name))
}
fn source(kind: &str) -> catalog_metadata::Source {
    catalog_metadata::Source {
        kind: kind.into(),
        locator: kind.as_bytes().to_vec(),
        display: "fixture".into(),
        ambiguous: false,
        provenance: serde_json::json!({"exact":9007199254740993u64,"opaque":"x".repeat(4000)}),
    }
}
fn input(rating: u8) -> xmp_packets::Inspection {
    let bytes=format!(r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:u="urn:retained" xmp:Rating="{rating}"><u:opaque>unknown</u:opaque></rdf:Description></rdf:RDF>"#).into_bytes();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    xmp_packets::Inspection {
        revision: xmp_packets::SourceRevision {
            length: bytes.len() as u64,
            blake3: hash.clone(),
            modified_unix_ns: None,
        },
        status: xmp_packets::Status::Complete,
        issues: vec![],
        packets: vec![xmp_packets::Packet {
            container: xmp_packets::Container::Sidecar,
            bytes: bytes.clone(),
            blake3: hash.clone(),
            ranges: vec![],
            group: "fixture".into(),
            attributes: BTreeMap::new(),
        }],
        parse_inputs: vec![xmp_packets::ParseInput {
            bytes,
            blake3: hash,
            packet_indices: vec![0],
            transformation: xmp_packets::Transformation::Identity,
            group: "fixture".into(),
        }],
    }
}
fn run(c: &mut Catalog, r: Request) -> Result<Response> {
    execute(c, r, &Limits::default()).map_err(|e| anyhow::anyhow!("{e:?}"))
}
fn identity(c: &mut Catalog, key: &VariantKey) -> Result<ImageIdentity> {
    let Response::Identity(i) = run(c, Request::Identity { key: key.clone() })? else {
        unreachable!()
    };
    Ok(i)
}
fn collect_candidates(
    c: &mut Catalog,
    i: &ImageIdentity,
    field: &str,
    b: &Limits,
) -> Result<Vec<Candidate>> {
    let mut result = vec![];
    let mut after = None;
    for _ in 0..1000 {
        let Response::Candidates(p) = execute(
            c,
            Request::Candidates {
                identity: i.clone(),
                field: field.into(),
                after,
                limit: 1,
            },
            b,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        else {
            unreachable!()
        };
        assert!(p.scanned.0 <= b.scan_rows as u64);
        result.extend(p.rows);
        after = p.next;
        if after.is_none() {
            return Ok(result);
        }
    }
    anyhow::bail!("candidate cursor did not exhaust")
}
#[test]
fn selected_copy_resolution_cas_and_opaque_provenance_preserve_originals() -> Result<()> {
    let (_temp, mut c, key) = fixture()?;
    let first = c.retain_metadata_for_image(&key, &source("catalog"), &input(2))?;
    let copy = c.create_edit_variant(&key, 0, "copy")?.key;
    let second = c.retain_metadata_for_image(&copy, &source("sidecar"), &input(4))?;
    let mut relocated = input(4);
    relocated.revision.modified_unix_ns = Some(9007199254740993);
    let relocated_observation =
        c.retain_metadata_for_image(&copy, &source("sidecar"), &relocated)?;
    assert_eq!(relocated_observation.observation_id, second.observation_id);
    let i = identity(&mut c, &copy)?;
    let limits = Limits {
        page_rows: 1,
        scan_rows: 1,
        ..Limits::default()
    };
    let candidates = collect_candidates(&mut c, &i, "rating", &limits)?;
    assert_eq!(candidates.len(), 2);
    let Response::Fields(fields) = run(
        &mut c,
        Request::Fields {
            identity: i.clone(),
            after: None,
            limit: 100,
        },
    )?
    else {
        unreachable!()
    };
    assert!(
        fields
            .rows
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .conflicted
    );
    let Response::Observations(p) = run(
        &mut c,
        Request::Observations {
            identity: i.clone(),
            after: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    let retained = &p.rows[0].provenance;
    assert!(retained.inline.is_none());
    let mut bytes = vec![];
    let mut offset = U64(0);
    loop {
        let Response::Chunk(ch) = run(
            &mut c,
            Request::TextChunk {
                identity: i.clone(),
                reference: retained.reference.clone(),
                offset,
                length: 257,
            },
        )?
        else {
            unreachable!()
        };
        bytes.extend(ch.bytes);
        let Some(next) = ch.next else { break };
        offset = next;
    }
    assert!(String::from_utf8(bytes)?.contains("9007199254740993"));
    let Response::Models(models) = run(
        &mut c,
        Request::Models {
            identity: i.clone(),
            observation: I64(first.observation_id),
            after: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(models.rows[0].id.0, first.model_ids[0]);
    let Response::Packets(packets) = run(
        &mut c,
        Request::Packets {
            identity: i.clone(),
            observation: I64(first.observation_id),
            after: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(
        packets.rows[0].bytes.0,
        input(2).packets[0].bytes.len() as u64
    );
    let Response::FileInstances(instances) = run(
        &mut c,
        Request::FileInstances {
            identity: i.clone(),
            after: None,
            limit: 100,
        },
    )?
    else {
        unreachable!()
    };
    assert!(!instances.rows.is_empty());
    assert!(instances.rows.iter().all(|f| f.observed_at.bytes.0 > 0));
    let old = c.metadata_for_image(&copy)?.revision;
    let Response::Changed { revision } = run(
        &mut c,
        Request::Resolve {
            key: copy.clone(),
            expected_revision: I64(old),
            field: "rating".into(),
            model: I64(second.model_ids[0]),
        },
    )?
    else {
        unreachable!()
    };
    assert!(revision.0 > old);
    assert!(
        execute(
            &mut c,
            Request::Resolve {
                key: copy.clone(),
                expected_revision: I64(old),
                field: "rating".into(),
                model: I64(first.model_ids[0])
            },
            &limits
        )
        .is_err()
    );
    assert!(
        execute(
            &mut c,
            Request::Fields {
                identity: i.clone(),
                after: None,
                limit: 1
            },
            &limits
        )
        .is_err()
    );
    let new = identity(&mut c, &copy)?;
    assert!(
        execute(
            &mut c,
            Request::Observations {
                identity: new.clone(),
                after: p.next,
                limit: 1
            },
            &limits
        )
        .is_err()
    );
    let master = c.metadata_for_image(&key)?;
    assert_eq!(
        master
            .fields
            .iter()
            .find(|f| f.name == "rating")
            .unwrap()
            .value,
        Some(crate::xmp::Value::Text("2".into()))
    );
    assert_eq!(
        c.metadata_packets_for_image(&copy, first.observation_id)?[0].bytes,
        input(2).packets[0].bytes
    );
    let Response::Decisions(decisions) = run(
        &mut c,
        Request::Decisions {
            identity: new,
            after: None,
            limit: 100,
        },
    )?
    else {
        unreachable!()
    };
    assert!(!decisions.rows.is_empty());
    Ok(())
}
#[test]
fn exact_utf16_packet_chunks_ownership_checksum_and_cancel() -> Result<()> {
    let (_temp, mut c, key) = fixture()?;
    let foreign = asset(&mut c, "foreign")?;
    let mut value = input(3);
    let mut raw = vec![255, 254];
    raw.extend(
        "<x>retained Ω 😀</x>"
            .repeat(3000)
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    value.packets[0].bytes = raw.clone();
    value.packets[0].blake3 = blake3::hash(&raw).to_hex().to_string();
    let retained = c.retain_metadata_for_image(&key, &source("sidecar"), &value)?;
    let source = BlobSource::Packet {
        observation: I64(retained.observation_id),
        ordinal: I64(0),
    };
    let mut actual = vec![];
    let mut offset = U64(0);
    loop {
        let Response::Chunk(ch) = run(
            &mut c,
            Request::BlobChunk {
                key: key.clone(),
                source: source.clone(),
                offset,
                length: CHUNK as u32,
            },
        )?
        else {
            unreachable!()
        };
        assert!(ch.verified);
        assert_eq!(ch.inspected_bytes.0, raw.len() as u64);
        assert!(ch.bytes.len() <= CHUNK);
        actual.extend(ch.bytes);
        let Some(next) = ch.next else { break };
        offset = next;
    }
    assert_eq!(actual, raw);
    assert!(
        execute(
            &mut c,
            Request::BlobChunk {
                key: foreign,
                source: source.clone(),
                offset: U64(0),
                length: 10
            },
            &Limits::default()
        )
        .is_err()
    );
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(
        execute_cancellable(
            &mut c,
            Request::BlobChunk {
                key: key.clone(),
                source: source.clone(),
                offset: U64(0),
                length: 10
            },
            &Limits::default(),
            &cancel
        )
        .is_err()
    );
    let hash = &value.packets[0].blake3;
    let compressed: i64 = c.db.query_row(
        "SELECT length(compressed) FROM metadata_blobs WHERE hash=?1",
        [hash],
        |r| r.get(0),
    )?;
    let cancel = Cancellation::default();
    let mut reader = BlobReader {
        db: &c.db,
        hash,
        offset: 0,
        length: compressed as usize,
        buffer: vec![],
        consumed: 0,
        cancel: &cancel,
    };
    let mut out = [0u8; 8];
    assert_eq!(reader.read(&mut out)?, 8);
    assert!(reader.buffer.len() <= CHUNK);
    cancel.cancel();
    assert!(reader.read(&mut out).is_err());
    drop(reader);
    c.db.execute(
        "UPDATE metadata_blobs SET compressed=zeroblob(length(compressed)) WHERE hash=?1",
        [hash],
    )?;
    assert!(
        execute(
            &mut c,
            Request::BlobChunk {
                key,
                source,
                offset: U64(0),
                length: 10
            },
            &Limits::default()
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn response_byte_cut_cursors_and_indexed_absent_field_scan() -> Result<()> {
    let (_temp, mut c, key) = fixture()?;
    for index in 0..12 {
        c.retain_metadata_for_image(
            &key,
            &source(&format!("source-{index}")),
            &input((index % 5) as u8),
        )?;
    }
    let i = identity(&mut c, &key)?;
    let limits = Limits {
        page_rows: 100,
        scan_rows: 100,
        page_bytes: 6000,
        ..Limits::default()
    };
    let mut after = None;
    let mut ids = vec![];
    for _ in 0..30 {
        let Response::Sources(p) = execute(
            &mut c,
            Request::Sources {
                identity: i.clone(),
                after,
                limit: 100,
            },
            &limits,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        else {
            unreachable!()
        };
        assert!(!p.rows.is_empty());
        ids.extend(p.rows.into_iter().map(|r| r.id.0));
        after = p.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 12);
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    let limits = Limits {
        page_rows: 1,
        scan_rows: 1,
        ..Limits::default()
    };
    assert!(collect_candidates(&mut c, &i, "absent", &limits)?.is_empty());
    for query in [
        "SELECT source_id FROM metadata_image_sources WHERE image_id='synthetic' AND source_id>0 ORDER BY source_id LIMIT 1",
        "SELECT id FROM metadata_models WHERE observation_id=1 AND ordinal>-1 ORDER BY ordinal LIMIT 1",
        "SELECT m.id,m.ordinal,m.observation_id,(a.association='ambiguous' OR o.status!='Complete'),v.semantic_hash FROM metadata_image_sources a JOIN metadata_observations o ON o.id=a.current_observation JOIN metadata_models m ON m.observation_id=a.current_observation LEFT JOIN metadata_values v ON v.model_id=m.id AND v.field='absent' WHERE a.image_id='synthetic' AND a.source_id=1 AND m.ordinal>-1 ORDER BY m.ordinal LIMIT 1",
        "SELECT id FROM metadata_file_instances WHERE asset_id='synthetic' AND id>0 ORDER BY id LIMIT 1",
    ] {
        let mut stmt = c.db.prepare(&format!("EXPLAIN QUERY PLAN {query}"))?;
        let plan = stmt
            .query_map([], |r| r.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .join(" ");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        assert!(plan.contains("SEARCH"), "{plan}");
        let mut statement = c.db.prepare(query)?;
        {
            let mut rows = statement.query([])?;
            while rows.next()?.is_some() {}
        }
        assert_eq!(
            statement.get_status(rusqlite::StatementStatus::FullscanStep),
            0
        );
        assert_eq!(statement.get_status(rusqlite::StatementStatus::Sort), 0);
        assert!(statement.get_status(rusqlite::StatementStatus::VmStep) < 128);
    }
    assert!(
        execute(
            &mut c,
            Request::Fields {
                identity: i,
                after: None,
                limit: 101
            },
            &Limits::default()
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn real_import_history_typed_fields_adobe_and_sibling_isolation() -> Result<()> {
    let mut fixture = imported::ImportedFixture::new(false, true)?;
    let key = fixture.keys[0].clone();
    let sibling = fixture.keys[1].clone();
    let c = &mut fixture.catalog;
    let Response::ImportHistory(root) = run(
        c,
        Request::ImportHistory {
            key: key.clone(),
            anchor_json: None,
            direction: history::Direction::Outgoing,
            after_json: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert!(!root.adobe_rendering_equivalent);
    let root_anchor = root.anchor_json.clone();
    assert!(
        execute(
            c,
            Request::ImportFields {
                key: sibling,
                anchor_json: root_anchor.clone(),
                role: RecordRole::Row,
                after: String::new(),
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    let mut relations = vec![];
    let mut after = None;
    for _ in 0..50 {
        let Response::ImportHistory(p) = run(
            c,
            Request::ImportHistory {
                key: key.clone(),
                anchor_json: Some(root_anchor.clone()),
                direction: history::Direction::Incoming,
                after_json: after,
                limit: 1,
            },
        )?
        else {
            unreachable!()
        };
        assert!(p.relations.len() <= 1);
        relations.extend(p.relations);
        after = p.next;
        if after.is_none() {
            break;
        }
    }
    assert!(relations.iter().any(|r| {
        r.source
            .as_ref()
            .is_some_and(|r| r.classification == history::Classification::History)
    }));
    let history = relations
        .iter()
        .find(|r| r.source.as_ref().is_some_and(|r| r.source_id == "h-40"))
        .unwrap();
    let anchor = history.anchor_json.clone().unwrap();
    let Response::AdobeProperties(adobe) = run(
        c,
        Request::AdobeProperties {
            key: key.clone(),
            anchor_json: anchor.clone(),
            column: "text".into(),
            settings_path_json: "[]".into(),
            after: U64(0),
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert!(!adobe.adobe_rendering_equivalent);
    assert_eq!(adobe.properties.rows.len(), 1);
    assert!(adobe.properties.next.is_some());
    let mut field_after = String::new();
    let mut names = vec![];
    loop {
        let Response::ImportFields(p) = run(
            c,
            Request::ImportFields {
                key: key.clone(),
                anchor_json: anchor.clone(),
                role: RecordRole::Row,
                after: field_after,
                limit: 1,
            },
        )?
        else {
            unreachable!()
        };
        names.extend(p.rows.into_iter().map(|f| f.name));
        let Some(next) = p.next else { break };
        field_after = next;
    }
    assert!(names.contains(&"cells_json".to_string()));
    let huge = relations
        .iter()
        .find(|r| r.source.as_ref().is_some_and(|r| r.source_id == "h-70"))
        .unwrap();
    let huge_anchor = huge.anchor_json.clone().unwrap();
    let Response::ImportFields(fields) = run(
        c,
        Request::ImportFields {
            key: key.clone(),
            anchor_json: huge_anchor.clone(),
            role: RecordRole::Row,
            after: String::new(),
            limit: 100,
        },
    )?
    else {
        unreachable!()
    };
    let cells = fields.rows.iter().find(|f| f.name == "cells_json").unwrap();
    assert!(cells.bytes.0 > 8 * 1024 * 1024);
    let Response::Chunk(bytes) = run(
        c,
        Request::ImportChunk {
            key: key.clone(),
            anchor_json: huge_anchor.clone(),
            role: RecordRole::Row,
            field: "cells_json".into(),
            offset: U64(1024 * 1024 - 7),
            length: 100,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(bytes.bytes.len(), 7);
    assert_eq!(bytes.next.unwrap().0, 1024 * 1024);
    assert!(bytes.verified);
    let Response::AdobeProperties(huge_adobe) = run(
        c,
        Request::AdobeProperties {
            key,
            anchor_json: huge_anchor,
            column: "text".into(),
            settings_path_json: "[]".into(),
            after: U64(0),
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert!(huge_adobe.properties.rows.is_empty());
    assert!(!huge_adobe.adobe_rendering_equivalent);
    c.db.execute(
        "UPDATE migration_retained_records SET compressed=zeroblob(?2) WHERE sequence=?1",
        params![root.row.record.0, 8 * 1024 * 1024 + 32769],
    )?;
    let failure = c
        .migration_variant_evidence(&root.key, None, 1)
        .unwrap_err()
        .to_string();
    assert!(
        failure.contains("stored record/seal byte bound"),
        "{failure}"
    );
    Ok(())
}

#[test]
fn packet_maximum_cost_and_oversized_transport_admission() -> Result<()> {
    let (_temp, mut c, key) = fixture()?;
    let mut value = input(3);
    value.packets[0].bytes = vec![b'x'; RAW_MAX];
    value.packets[0].blake3 = blake3::hash(&value.packets[0].bytes).to_hex().to_string();
    let retained = c.retain_metadata_for_image(&key, &source("sidecar"), &value)?;
    let packet = BlobSource::Packet {
        observation: I64(retained.observation_id),
        ordinal: I64(0),
    };
    let Response::Chunk(chunk) = run(
        &mut c,
        Request::BlobChunk {
            key: key.clone(),
            source: packet.clone(),
            offset: U64((RAW_MAX - 3) as u64),
            length: 16,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(chunk.bytes, b"xxx");
    assert_eq!(chunk.inspected_bytes.0, RAW_MAX as u64);
    assert!(chunk.verified);
    assert!(chunk.next.is_none());
    assert!(
        execute(
            &mut c,
            Request::BlobChunk {
                key: key.clone(),
                source: packet.clone(),
                offset: U64(0),
                length: (CHUNK + 1) as u32
            },
            &Limits::default()
        )
        .is_err()
    );
    assert!(
        execute(
            &mut c,
            Request::BlobChunk {
                key: key.clone(),
                source: packet.clone(),
                offset: U64((RAW_MAX + 1) as u64),
                length: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    // Corrupt storage size is rejected before the compressed blob is selected.
    c.db.execute(
        "UPDATE metadata_blobs SET compressed=zeroblob(?2) WHERE hash=?1",
        params![value.packets[0].blake3, (RAW_MAX + 65537) as i64],
    )?;
    assert!(
        execute(
            &mut c,
            Request::BlobChunk {
                key,
                source: packet,
                offset: U64(0),
                length: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn copied_import_history_keeps_selected_authority_and_packet_snapshot() -> Result<()> {
    let mut fixture = imported::ImportedFixture::new(false, false)?;
    let imported = fixture.keys[0].clone();
    let sibling = fixture.keys[1].clone();
    let c = &mut fixture.catalog;
    let first = c.retain_metadata_for_image(&imported, &source("catalog"), &input(2))?;
    let copy = c.create_edit_variant(&imported, 0, "native copy")?.key;
    let second = c.retain_metadata_for_image(&imported, &source("catalog"), &input(4))?;
    let Response::ImportHistory(parent) = run(
        c,
        Request::ImportHistory {
            key: imported.clone(),
            anchor_json: None,
            direction: history::Direction::Incoming,
            after_json: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    let Response::ImportHistory(page) = run(
        c,
        Request::ImportHistory {
            key: copy.clone(),
            anchor_json: None,
            direction: history::Direction::Incoming,
            after_json: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(page.key, copy);
    assert_eq!(page.row.record.0, parent.row.record.0);
    assert_ne!(page.anchor_json, parent.anchor_json);
    assert!(
        execute(
            c,
            Request::ImportHistory {
                key: copy.clone(),
                anchor_json: Some(parent.anchor_json),
                direction: history::Direction::Incoming,
                after_json: None,
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    assert!(
        execute(
            c,
            Request::ImportHistory {
                key: sibling,
                anchor_json: Some(page.anchor_json.clone()),
                direction: history::Direction::Incoming,
                after_json: None,
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    let Response::Chunk(bytes) = run(
        c,
        Request::BlobChunk {
            key: copy.clone(),
            source: BlobSource::Packet {
                observation: I64(first.observation_id),
                ordinal: I64(0),
            },
            offset: U64(0),
            length: CHUNK as u32,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(bytes.bytes, input(2).packets[0].bytes);
    assert!(
        execute(
            c,
            Request::BlobChunk {
                key: copy.clone(),
                source: BlobSource::Packet {
                    observation: I64(second.observation_id),
                    ordinal: I64(0)
                },
                offset: U64(0),
                length: 10
            },
            &Limits::default()
        )
        .is_err()
    );
    let native = asset(c, "unimported")?;
    assert!(
        execute(
            c,
            Request::ImportHistory {
                key: native,
                anchor_json: None,
                direction: history::Direction::Incoming,
                after_json: None,
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    let mut deep = copy.clone();
    for _ in 0..64 {
        deep = c.create_edit_variant(&deep, 0, "copy depth")?.key;
    }
    let failure = c
        .migration_variant_evidence(&deep, None, 1)
        .unwrap_err()
        .to_string();
    assert!(failure.contains("exceeds 64"), "{failure}");
    // Real schema prevents this mutation. Disable its guards only inside this
    // disposable fixture to prove read-side failure on corrupt ancestry.
    c.db.execute_batch(
        "DROP TRIGGER image_identity_immutable; PRAGMA ignore_check_constraints=ON;",
    )?;
    c.db.execute("UPDATE catalog_images SET copied_from_sequence=sequence WHERE asset_id=?1 AND variant_id=?2",params![copy.asset_id,copy.variant_id])?;
    let failure = c
        .migration_variant_evidence(&copy, None, 1)
        .unwrap_err()
        .to_string();
    assert!(failure.contains("cycle"), "{failure}");
    Ok(())
}

#[test]
fn import_columns_lists_actual_names_types_and_oversized_unknowns() -> Result<()> {
    let mut fixture = imported::ImportedFixture::new(false, true)?;
    let key = fixture.keys[0].clone();
    let other = fixture.keys[1].clone();
    let c = &mut fixture.catalog;
    let Response::ImportHistory(first) = run(
        c,
        Request::ImportHistory {
            key: key.clone(),
            anchor_json: None,
            direction: history::Direction::Incoming,
            after_json: None,
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    let root_anchor = first.anchor_json.clone();
    let mut relations = first.relations;
    let mut after = first.next;
    while let Some(cursor) = after {
        let Response::ImportHistory(page) = run(
            c,
            Request::ImportHistory {
                key: key.clone(),
                anchor_json: Some(root_anchor.clone()),
                direction: history::Direction::Incoming,
                after_json: Some(cursor),
                limit: 1,
            },
        )?
        else {
            unreachable!()
        };
        relations.extend(page.relations);
        after = page.next;
        assert!(relations.len() < 100);
    }
    let find = |id: &str| {
        relations
            .iter()
            .find(|r| r.source.as_ref().is_some_and(|s| s.source_id == id))
            .unwrap()
            .anchor_json
            .clone()
            .unwrap()
    };
    let anchor = find("h-40");
    let huge = find("h-70");
    let mut after = U64(0);
    let mut columns = Vec::new();
    loop {
        let Response::ImportColumns(page) = run(
            c,
            Request::ImportColumns {
                key: key.clone(),
                anchor_json: anchor.clone(),
                after,
                limit: 1,
            },
        )?
        else {
            unreachable!()
        };
        assert!(page.types_complete);
        assert_eq!(page.columns.rows.len(), 1);
        columns.extend(page.columns.rows);
        let Some(next) = page.columns.next else { break };
        after = U64(next.parse()?);
    }
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id_local", "image", "text"]
    );
    assert_eq!(
        columns.iter().map(|c| c.ordinal.0).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        columns
            .iter()
            .map(|c| c.cell_type.as_deref())
            .collect::<Vec<_>>(),
        [Some("integer"), Some("integer"), Some("text")]
    );
    assert!(columns[2].bytes.unwrap().0 > 0);
    let Response::AdobeProperties(adobe) = run(
        c,
        Request::AdobeProperties {
            key: key.clone(),
            anchor_json: anchor.clone(),
            column: columns[2].name.clone(),
            settings_path_json: "[]".into(),
            after: U64(0),
            limit: 1,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(adobe.properties.rows.len(), 1);
    let Response::ImportColumns(page) = run(
        c,
        Request::ImportColumns {
            key: key.clone(),
            anchor_json: huge.clone(),
            after: U64(0),
            limit: 100,
        },
    )?
    else {
        unreachable!()
    };
    assert!(!page.types_complete);
    assert!(page.reason.contains("8 MiB"));
    assert_eq!(page.columns.rows.len(), 3);
    assert!(
        page.columns
            .rows
            .iter()
            .all(|c| c.cell_type.is_none() && c.bytes.is_none())
    );
    let Response::Chunk(raw) = run(
        c,
        Request::ImportChunk {
            key: key.clone(),
            anchor_json: huge,
            role: RecordRole::Row,
            field: "cells_json".into(),
            offset: U64(0),
            length: 128,
        },
    )?
    else {
        unreachable!()
    };
    assert_eq!(raw.bytes.len(), 128);
    assert!(raw.next.is_some());
    assert!(
        execute(
            c,
            Request::ImportColumns {
                key: other,
                anchor_json: anchor.clone(),
                after: U64(0),
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    assert!(
        execute(
            c,
            Request::ImportColumns {
                key,
                anchor_json: anchor,
                after: U64(4),
                limit: 1
            },
            &Limits::default()
        )
        .is_err()
    );
    let excessive = serde_json::to_vec(&vec![crate::lightroom::plan::Cell::Null; 4097])?;
    assert!(limited_list::<crate::lightroom::plan::Cell>(&excessive, 4096).is_err());
    Ok(())
}
