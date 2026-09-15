use super::*;
use crate::lightroom::migration_source::ReadLimits;
const SEAL: &str = r#"[1,{"units":[0,255],"encoding":"UnixBytes"},["object",1,null,"changed"],"",["","",""],[],[]]"#;
fn body(seal: &str) -> Result<String> {
    Ok(format!(
        r#"{{"seal":{seal},"limits":{},"protected":[]}}"#,
        serde_json::to_string(&SqlLimits::from(ReadLimits::default()))?
    ))
}
fn parity(raw: &str, positive: bool) -> Result<()> {
    let old = serde_json::from_str::<Authority>(raw);
    let new = decode(raw.as_bytes(), &|| false);
    assert_eq!(
        old.is_ok(),
        positive,
        "public outcome for {raw}: {:?}",
        old.as_ref().err()
    );
    assert_eq!(
        new.is_ok(),
        positive,
        "projected outcome for {raw}: {:?}",
        new.as_ref().err()
    );
    if let (Ok(old), Ok(new)) = (old, new) {
        assert_eq!(serde_json::to_vec(&old)?, serde_json::to_vec(&new)?);
        assert_eq!(old.binding()?, new.binding()?);
    }
    Ok(())
}
#[test]
fn authority_order_modes_sequences_and_u128_preserve_public_grammar() -> Result<()> {
    let b = body(SEAL)?;
    for raw in [
        format!(r#"{{"mode":"Sql","authority":{b}}}"#),
        format!(r#"{{"authority":{b},"mode":"Sql"}}"#),
        format!(r#"["Sql",{b}]"#),
        format!(r#"["S\u0071l",{b}]"#),
    ] {
        parity(&raw, true)?;
    }
    let limits = serde_json::to_string(&SqlLimits::from(ReadLimits::default()))?;
    parity(&format!(r#"["Sql",[{SEAL},{limits},[]]]"#), false)?;
    parity(
        &format!(r#"{{"mode":{{"Sql":null}},"authority":{b}}}"#),
        true,
    )?;
    parity(&format!(r#"[{{"Sql":null}},{b}]"#), false)?;
    for ns in [
        "0",
        "18446744073709551616",
        "340282366920938463463374607431768211455",
    ] {
        let s = SEAL.replace(",null,\"changed\"", &format!(",{ns},\"changed\""));
        assert!(s.contains(ns));
        let b = body(&s)?;
        parity(&format!(r#"{{"mode":"Sql","authority":{b}}}"#), true)?;
        parity(&format!(r#"{{"authority":{b},"mode":"Sql"}}"#), false)?;
    }
    for raw in [
        format!(r#"{{"mode":"Sql","mode":"Sql","authority":{b}}}"#),
        format!(r#"{{"mode":"Sql","authority":{b},"authority":{b}}}"#),
        format!(r#"{{"unknown":0,"mode":"Sql","authority":{b}}}"#),
        format!(r#"["Sql",{b},null]"#),
        r#"{"mode":"Sql","authority":null}"#.into(),
    ] {
        parity(&raw, false)?;
    }
    Ok(())
}
#[test]
fn authority_buffered_scalar_checks_do_not_build_content_graphs() -> Result<()> {
    // Unknown identity fields are ignored by direct typed serde but every
    // scalar is visited when old adjacent-tag content arrives before its mode.
    let identity = r#"{"object":"o","bytes":1,"modified_ns":null,"changed":"c","unknown":1e9999}"#;
    let s = SEAL.replace(r#"["object",1,null,"changed"]"#, identity);
    let b = body(&s)?;
    parity(&format!(r#"{{"mode":"Sql","authority":{b}}}"#), true)?;
    parity(&format!(r#"{{"authority":{b},"mode":"Sql"}}"#), false)?;
    for depth in [20, 140] {
        let nested = "[".repeat(depth) + "0" + &"]".repeat(depth);
        let s = s.replace("1e9999", &nested);
        let b = body(&s)?;
        parity(&format!(r#"{{"mode":"Sql","authority":{b}}}"#), true)?;
        parity(&format!(r#"{{"authority":{b},"mode":"Sql"}}"#), depth == 20)?;
    }
    // Compare every depth around the pinned serde_json boundary, retaining the
    // direct public decoder as grammar oracle rather than guessing a cutoff.
    for depth in 118..=132 {
        let nested = "[".repeat(depth) + "0" + &"]".repeat(depth);
        let candidate = s.replace("1e9999", &nested);
        let b = body(&candidate)?;
        let raw = format!(r#"{{"authority":{b},"mode":"Sql"}}"#);
        let accepted = serde_json::from_str::<Authority>(&raw).is_ok();
        parity(&raw, accepted)?;
        println!("C buffered authority nested depth={depth} accepted={accepted}");
    }
    // Content is not Value and does not interpret this special property.
    let b = body(&s.replace(
        "\"unknown\":1e9999",
        r#""$serde_json::private::RawValue":"{}""#,
    ))?;
    parity(&format!(r#"{{"authority":{b},"mode":"Sql"}}"#), true)?;
    Ok(())
}
#[test]
fn authority_protected_count_and_cancel_precede_source_opens() -> Result<()> {
    let key = r#"{"volume":"18446744073709551615","index":"0"}"#;
    for n in [4096, 4097] {
        let b = body(SEAL)?.replace(
            "\"protected\":[]",
            &format!("\"protected\":[{}]", vec![key; n].join(",")),
        );
        let raw = format!(r#"{{"mode":"Sql","authority":{b}}}"#);
        let result = decode(raw.as_bytes(), &|| false);
        assert_eq!(result.is_ok(), n == 4096);
        if let Ok(Authority::Sql { protected, .. }) = result {
            assert_eq!(protected.capacity(), 4096);
        }
        assert!(decode(raw.as_bytes(), &|| true).is_err());
    }
    Ok(())
}

#[test]
fn capture_sql_authority_is_distinct_and_binding_complete() -> Result<()> {
    use crate::{
        application::U64,
        lightroom::{PAGE_BYTES, source::Revision},
        lightroom_migration_worker::source_reader::capture_wire,
        storage_volume::NativePath,
    };
    let mut value = capture_wire::Authority {
        protocol: 1,
        build: crate::lightroom_migration_worker::worker::build_identity().into(),
        workbench_instance: "w".into(),
        workbench_generation: "g".into(),
        filesystem_lease: "f".into(),
        operation: "o".into(),
        capture_generation: "c".into(),
        expires_unix_ms: U64(u64::MAX),
        capture_root: NativePath::from_path(std::path::Path::new("/capture")),
        member: capture_wire::MEMBER.into(),
        manifest_blake3: "1".repeat(64),
        revision_id: "2".repeat(64),
        logical_revision: Revision {
            object: "object".into(),
            bytes: 1,
            modified_ns: None,
            changed: "changed".into(),
        },
        logical_blake3: "3".repeat(64),
        maximum_bytes: U64(1),
        physical: FileKey {
            volume: U64(1),
            index: U64(2),
        },
        companion_generation: "e".into(),
        raw_roster_blake3: "4".repeat(64),
        limits: capture_wire::Limits {
            open_deadline_ms: U64(1000),
            total_deadline_ms: U64(2000),
            vm_steps: U64(1000),
            schema_objects: U64(16),
            schema_bytes: U64(PAGE_BYTES as u64),
            page_bytes: U64(PAGE_BYTES as u64),
            max_cell_bytes: U64(1024),
            result_bytes: U64(1024 * 1024),
            inline_bytes: U64(1024),
            chunk_bytes: U64(1024),
            max_rows: U64(100),
        },
        protected: vec![],
        binding_blake3: String::new(),
    };
    value.binding_blake3 = value.computed_binding()?;
    let encoded = serde_json::to_vec(&Authority::CaptureSql { value })?;
    assert!(matches!(
        decode(&encoded, &|| false)?,
        Authority::CaptureSql { .. }
    ));
    let wrong = String::from_utf8(encoded)?.replacen("CaptureSql", "Sql", 1);
    assert!(decode(wrong.as_bytes(), &|| false).is_err());
    let sql = format!(r#"{{"mode":"CaptureSql","authority":{}}}"#, body(SEAL)?);
    assert!(decode(sql.as_bytes(), &|| false).is_err());
    Ok(())
}

fn artifact_body(path: &str) -> Result<String> {
    let descriptor = format!(
        r#"{{"protocol":1,"request":{{"retained_capture_record":1,"member_index":0,"mapping":{{"root":{path},"relative":{path},"copy_identity":["copy",1,null,"c"]}}}},"selected_input":"a","capture_revision":"b","manifest_blake3":"c","artifact":{{"source":{path},"role":"main","relative":{path},"stored":"member","revision":["original",1,null,"c"],"blake3":"d"}}}}"#
    );
    Ok(format!(
        r#"{{"descriptor":{descriptor},"limits":{{"maximum_bytes":"1","open_deadline_ms":"1000","chunk_deadline_ms":"1000","chunk_bytes":"1"}},"protected":[]}}"#
    ))
}
fn orders(mode: &str, body: &str, direct: bool, buffered: bool) -> Result<()> {
    parity(
        &format!(r#"{{"mode":"{mode}","authority":{body}}}"#),
        direct,
    )?;
    parity(
        &format!(r#"{{"authority":{body},"mode":"{mode}"}}"#),
        buffered,
    )?;
    parity(&format!(r#"["{mode}",{body}]"#), direct)
}
#[test]
fn buffered_path_identifiers_and_unit_maps_preserve_both_authority_modes() -> Result<()> {
    for (index, encoding) in [(0, "UnixBytes"), (1, "WindowsWide")] {
        for (path, direct, buffered) in [
            (format!(r#"["{encoding}",[0,255]]"#), true, true),
            (format!(r#"[{index},[0,255]]"#), false, true),
            (
                format!(r#"{{"units":[0,255],"encoding":{{"{encoding}":{{}}}}}}"#),
                false,
                true,
            ),
            (
                format!(r#"{{"encoding":{{"{encoding}":null}},"units":[0,255]}}"#),
                true,
                true,
            ),
            (
                format!(r#"{{"encoding":{{"{encoding}":[]}},"units":[0,255]}}"#),
                false,
                false,
            ),
            (
                format!(r#"{{"encoding":{{"{encoding}":{{"x":0}}}},"units":[0,255]}}"#),
                false,
                false,
            ),
            (format!(r#"[{{"{encoding}":null}},[0,255]]"#), false, false),
        ] {
            let seal = SEAL.replace(r#"{"units":[0,255],"encoding":"UnixBytes"}"#, &path);
            orders("Sql", &body(&seal)?, direct, buffered)?;
            orders("Artifact", &artifact_body(&path)?, direct, buffered)?;
        }
    }
    for path in [
        "[2,[]]",
        "[-1,[]]",
        "[0.0,[]]",
        "[null,[]]",
        "[18446744073709551616,[]]",
        "[0,[256]]",
    ] {
        let seal = SEAL.replace(r#"{"units":[0,255],"encoding":"UnixBytes"}"#, path);
        orders("Sql", &body(&seal)?, false, false)?;
        orders("Artifact", &artifact_body(path)?, false, false)?;
    }
    Ok(())
}
#[test]
fn buffered_supplement_status_units_and_map_only_authority_bodies_match_public() -> Result<()> {
    for status in [
        "Absent",
        "Complete",
        "Unsupported",
        "Malformed",
        "ResourceLimit",
        "SourceChanged",
    ] {
        for (value, direct, buffered) in [
            (format!(r#""{status}""#), true, true),
            (format!(r#"{{"{status}":null}}"#), true, true),
            (format!(r#"{{"{status}":{{}}}}"#), false, true),
            (format!(r#"{{"{status}":[]}}"#), false, false),
            (format!(r#"{{"{status}":{{"x":0}}}}"#), false, false),
            ("0".into(), false, false),
            ("null".into(), false, false),
        ] {
            let pin = format!(
                r#"{{"revision":"r","source_id":"s","origin":"embedded","source_revision":[1,"digest",null],"historical_status":{value},"proof_blake3":"p"}}"#
            );
            let seal = SEAL[..SEAL.len() - 1].to_owned() + ",[" + &pin + "]]";
            orders("Sql", &body(&seal)?, direct, buffered)?;
        }
    }
    // Struct variants are map-only, even though nested ordinary InputSeal,
    // FileIdentity and ArtifactDescriptor structs retain sequence grammar.
    let limits = serde_json::to_string(&SqlLimits::from(ReadLimits::default()))?;
    orders("Sql", &format!("[{SEAL},{limits},[]]"), false, false)?;
    for mode in ["Sql", "Artifact"] {
        for b in ["[]", "null", "0"] {
            orders(mode, b, false, false)?;
        }
    }
    Ok(())
}
